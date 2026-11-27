//! Static dependencies of rules (formulas, checks, automatic transitions, `becomes`
//! conditions, postings, `frozen when`): which fields they read, and through which path
//! from the record they belong to.
//!
//! With storage on demand, a commit cannot re-evaluate every rule on every record. Given
//! the records a transaction wrote, the paths are walked backwards to the records whose
//! rules may give a different result. A read the analysis cannot place on a path (a whole
//! table scanned without a recognizable filter, an opaque call) makes the rule depend on
//! every record of its entity: slower, never wrong.

use quadra_ir::*;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Step {
    /// from a record of `ent`, follow its reference field `field`
    Ref { ent: String, field: String },
    /// to the records of `child` (an entity or a ledger) whose `back` points to the record
    Inv { child: String, back: String },
}

#[derive(Debug, Default, Clone)]
pub struct Deps {
    /// (path from the root record, entity or ledger read, field read)
    pub reads: BTreeSet<(Vec<Step>, String, String)>,
    /// entities and ledgers read without a path from the root: any change → every root
    pub globals: BTreeSet<String>,
    /// something the analysis cannot follow: any change → every root
    pub opaque: bool,
    /// reads the clock (`today()`, `now()`): also needs a periodic sweep
    pub time: bool,
    /// paths from the root to records whose same formula the root's formula reads (a formula
    /// defined by recursion over a tree: `depth = parent.depth + 1`): the value changes when the
    /// value on such a record changes (`analyze_formula`)
    pub recursive: BTreeSet<Vec<Step>>,
    /// reads something no write of a record changes, or that depends on who or where it is
    /// read (`me`, `current`, a per-company field, `updated_at`, `warnings`, a currency by its
    /// code, a counter): why, when it does. Such a value cannot be kept up to date by
    /// following writes (see `incremental`).
    pub volatile: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Path(Vec<Step>, String),
    Global(String),
    Table(String),
    /// a tuple whose fields may carry paths: `(rate: g.key, line: g.items.first())`, and the
    /// groups of `group_by` (`key`, `items`)
    Tuple(Vec<(Option<String>, Val)>),
    Other,
}

struct An<'m> {
    m: &'m Model,
    deps: Deps,
    depth: usize,
    /// the formulas being analyzed, outermost first (the first is the root's own, when the
    /// analysis is of a formula)
    stack: Vec<(String, String)>,
}

type Env = Vec<HashMap<String, Val>>;

/// Dependencies of expressions and statements evaluated on a record of `root`.
pub fn analyze(m: &Model, root: &str, exprs: &[&Expr], stmts: &[&[Stmt]]) -> Deps {
    let mut a = An {
        m,
        deps: Deps::default(),
        depth: 0,
        stack: Vec::new(),
    };
    let this = Val::Path(Vec::new(), root.to_string());
    for e in exprs {
        let mut env: Env = vec![HashMap::new()];
        a.expr(e, &mut env, &this);
    }
    for s in stmts {
        let mut env: Env = vec![HashMap::new()];
        a.stmts(s, &mut env, &this);
    }
    a.deps
}

/// Members of every record that change without a write of a field (or are read through the
/// record's history).
const VOLATILE_MEMBERS: &[&str] = &["updated_at", "updated_by", "version", "warnings"];
/// Members of every record that never change once it exists.
const IMMUTABLE_MEMBERS: &[&str] = &["id", "created_at", "created_by"];

/// Dependencies of the formula `ent.field`, with its recursive steps (`Deps::recursive`): a
/// formula reading itself further along a tree is followed one step, not inlined (where
/// `analyze` gives up and makes it opaque).
pub fn analyze_formula(m: &Model, ent: &str, field: &str) -> Deps {
    let Some(f) = m.entity(ent).and_then(|e| m.find_field(e, field)) else {
        return Deps::default();
    };
    let Some(formula) = &f.formula else {
        return Deps::default();
    };
    let mut a = An {
        m,
        deps: Deps::default(),
        depth: 0,
        stack: vec![(ent.to_string(), field.to_string())],
    };
    let this = Val::Path(Vec::new(), ent.to_string());
    let mut env: Env = vec![HashMap::new()];
    a.expr(formula, &mut env, &this);
    a.deps
}

/// Dependencies of a lambda's body evaluated on a record of `root` bound to `param` (the
/// contribution of one child to a rollup: `o => o.total` over a customer's orders).
pub fn analyze_lambda(m: &Model, root: &str, param: &str, body: &Expr) -> Deps {
    let mut a = An {
        m,
        deps: Deps::default(),
        depth: 0,
        stack: Vec::new(),
    };
    let mut env: Env = vec![HashMap::from([(
        param.to_string(),
        Val::Path(Vec::new(), root.to_string()),
    )])];
    a.expr(body, &mut env, &Val::Other);
    a.deps
}

/// Whether the materialized formula `ent.field` can be kept up to date by following the writes
/// of each commit, and so read back from its column instead of being computed: every value it
/// reads, transitively, is a stored field or a formula reached by a path the analysis follows
/// from the record (`Ok`). Otherwise why not: it reads the clock, whole entities, a ledger,
/// something the analysis cannot follow (a recursion, an opaque function), or a value that
/// depends on who reads it or changes without a write (`volatile`).
pub fn incremental(m: &Model, ent: &str, field: &str) -> Result<(), String> {
    let e = m.entity(ent).ok_or("no such entity")?;
    if e.kind != EntityKind::Entity || e.is_ledger || e.temporary {
        return Err("not a stored entity".into());
    }
    let f = m.find_field(e, field).ok_or("no such field")?;
    let formula = f.formula.as_ref().ok_or("not a formula")?;
    if f.per_company {
        return Err("a per-company formula".into());
    }
    let _ = formula;
    // the rows of a ledger posted to the record: kept by adding those of each commit
    if ledger_rollup(m, ent, field).is_some() {
        return Ok(());
    }
    let d = analyze_formula(m, ent, field);
    if d.opaque {
        return Err("it reads something the analysis cannot follow (a recursion, a function without a body, a metric)".into());
    }
    if d.time {
        return Err("it reads the clock".into());
    }
    if let Some(g) = d.globals.iter().next() {
        return Err(format!("it reads every {g}"));
    }
    if let Some(why) = d.volatile {
        return Err(format!("it {why}"));
    }
    for (path, e, _) in &d.reads {
        let ledger = |n: &str| m.entity(n).map_or(false, |x| x.is_ledger);
        if ledger(e)
            || path
                .iter()
                .any(|s| matches!(s, Step::Inv { child, .. } if ledger(child)))
        {
            return Err(format!("it reads the ledger {e}"));
        }
    }
    Ok(())
}

/// A formula that adds up (or counts) the rows of a ledger posted to the record:
/// `GL.where(g => g.account == self and g.company == company).sum(g => g.amount)`,
/// `Stock.count(s => s.product == self)`. One of the conditions is `row.key == self` (the
/// anchor: a key of the ledger referring to the record's entity); the others and the value
/// read only the row's own columns, the record's own stored fields and constants. Ledger rows
/// are never changed or removed (a correction is another row): the value changes only with
/// the rows each commit posts, or when a stored field of the record the conditions read is
/// written (then it is computed again).
#[derive(Debug, Clone)]
pub struct LedgerRollup<'m> {
    pub ledger: String,
    /// the key of the ledger holding the record
    pub key: String,
    /// the conditions on a row: (parameter, body)
    pub filters: Vec<(&'m str, &'m Expr)>,
    /// the value added up (None: the rows are counted)
    pub sum: Option<(&'m str, &'m Expr)>,
    /// the aggregate call (its typed zero)
    pub span: quadra_ir::Span,
}

/// `LedgerRollup` of a formula, when it has that shape.
pub fn ledger_rollup<'m>(m: &'m Model, ent: &str, field: &str) -> Option<LedgerRollup<'m>> {
    let owner = m.entity(ent)?;
    let f = m.find_field(owner, field)?;
    if f.per_company {
        return None;
    }
    let e = f.formula.as_ref()?;
    let ExprKind::Call { callee, args } = &e.kind else {
        return None;
    };
    let ExprKind::Field {
        base,
        name,
        optional: false,
    } = &callee.kind
    else {
        return None;
    };
    let lambda = |a: &'m quadra_ir::Arg| -> Option<(&'m str, &'m Expr)> {
        if a.name.is_some() || a.spread {
            return None;
        }
        match &a.value.kind {
            ExprKind::Lambda { params, body } if params.len() == 1 => {
                Some((params[0].as_str(), body.as_ref()))
            }
            _ => None,
        }
    };
    let mut filters: Vec<(&'m str, &'m Expr)> = Vec::new();
    let sum = match (name.as_str(), args.as_slice()) {
        ("sum", [a]) => Some(lambda(a)?),
        ("count", []) => None,
        ("count", [a]) => {
            filters.push(lambda(a)?);
            None
        }
        _ => return None,
    };
    let mut b: &'m Expr = base;
    let ledger = loop {
        match &b.kind {
            ExprKind::Call { callee, args } => {
                let ExprKind::Field {
                    base,
                    name,
                    optional: false,
                } = &callee.kind
                else {
                    return None;
                };
                let [a] = args.as_slice() else { return None };
                if name != "where" {
                    return None;
                }
                filters.push(lambda(a)?);
                b = base;
            }
            ExprKind::Name(n) => break n.clone(),
            _ => return None,
        }
    };
    let l = m.entity(&ledger).filter(|l| l.is_ledger)?;
    // the anchor: `p.key == self` among the conditions
    let mut key: Option<String> = None;
    for (p, body) in &filters {
        for c in conjuncts(body) {
            if let ExprKind::Binary {
                op: BinOp::Eq,
                lhs,
                rhs,
            } = &c.kind
            {
                let side = |x: &Expr, y: &Expr| -> Option<String> {
                    let ExprKind::Field { base, name, .. } = &x.kind else {
                        return None;
                    };
                    (matches!(&base.kind, ExprKind::Name(n) if n == p)
                        && matches!(y.kind, ExprKind::SelfRef))
                    .then(|| name.clone())
                };
                if let Some(k) = side(lhs, rhs).or_else(|| side(rhs, lhs)) {
                    let kf = l.fields.iter().find(|x| x.name == k)?;
                    let target = match &kf.ty {
                        Some(Ty::Path { segments, .. }) => segments.last().cloned(),
                        Some(Ty::Optional(i)) => match i.as_ref() {
                            Ty::Path { segments, .. } => segments.last().cloned(),
                            _ => None,
                        },
                        _ => None,
                    };
                    if target.as_deref() == Some(ent) && key.is_none() {
                        key = Some(k);
                    }
                }
            }
        }
    }
    let key = key?;
    // everything else reads the row's columns, the record's stored fields, constants
    for (p, body) in filters.iter().chain(sum.iter()) {
        if !row_local(m, owner, l, p, body, true) {
            return None;
        }
    }
    Some(LedgerRollup {
        ledger,
        key,
        filters,
        sum,
        span: e.span,
    })
}

/// Whether `e` reads only columns of the ledger row bound to `p` (`p.col`), stored fields of
/// the record (`company`), constants and `self` compared with a row column (`anchor`).
fn row_local(
    m: &Model,
    owner: &quadra_ir::Entity,
    l: &quadra_ir::Entity,
    p: &str,
    e: &Expr,
    anchor: bool,
) -> bool {
    let rec = |x: &Expr| row_local(m, owner, l, p, x, anchor);
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Quantity { .. } => true,
        ExprKind::Name(n) if n == p => false,
        ExprKind::Name(n) => match m.find_field(owner, n) {
            Some(f) => {
                f.formula.is_none() && !f.per_company && f.kind == quadra_ir::FieldKind::Stored
            }
            None => m.variant(n).is_some_and(|v| v.fields.is_empty()),
        },
        ExprKind::Field { base, name, .. } => {
            matches!(&base.kind, ExprKind::Name(n) if n == p)
                && l.fields.iter().any(|f| f.name == *name)
        }
        ExprKind::Binary {
            op: BinOp::Eq | BinOp::Ne,
            lhs,
            rhs,
        } if anchor
            && (matches!(lhs.kind, ExprKind::SelfRef) || matches!(rhs.kind, ExprKind::SelfRef)) =>
        {
            let other = if matches!(lhs.kind, ExprKind::SelfRef) {
                rhs
            } else {
                lhs
            };
            matches!(&other.kind, ExprKind::Field { base, name, .. } if matches!(&base.kind, ExprKind::Name(n) if n == p) && l.fields.iter().any(|f| f.name == *name))
        }
        ExprKind::Binary { lhs, rhs, .. } => rec(lhs) && rec(rhs),
        ExprKind::Unary { expr, .. } => rec(expr),
        ExprKind::If {
            cond,
            then,
            otherwise,
        } => rec(cond) && rec(then) && rec(otherwise),
        ExprKind::Set(items) => items.iter().all(rec),
        ExprKind::List(items) => items.iter().all(|a| !a.spread && rec(&a.value)),
        _ => false,
    }
}

fn lookup(env: &Env, n: &str) -> Option<Val> {
    env.iter().rev().find_map(|s| s.get(n).cloned())
}

fn push(p: &[Step], s: Step) -> Vec<Step> {
    let mut v = p.to_vec();
    v.push(s);
    v
}

impl<'m> An<'m> {
    fn entity_of(&self, v: &Val) -> Option<String> {
        match v {
            Val::Path(_, t) | Val::Global(t) | Val::Table(t) => Some(t.clone()),
            Val::Tuple(_) | Val::Other => None,
        }
    }

    fn stmts(&mut self, ss: &[Stmt], env: &mut Env, this: &Val) -> Val {
        env.push(HashMap::new());
        let mut last = Val::Other;
        for s in ss {
            last = Val::Other;
            match &s.kind {
                StmtKind::Let { name, value, .. } => {
                    let v = self.expr(value, env, this);
                    env.last_mut().unwrap().insert(name.clone(), v);
                }
                StmtKind::If {
                    binding,
                    cond,
                    then,
                    otherwise,
                } => {
                    let c = self.expr(cond, env, this);
                    env.push(HashMap::new());
                    if let Some(b) = binding {
                        env.last_mut().unwrap().insert(b.clone(), c);
                    }
                    self.stmts(then, env, this);
                    env.pop();
                    if let Some(o) = otherwise {
                        self.stmts(o, env, this);
                    }
                }
                StmtKind::For { names, iter, body } => {
                    let it = self.expr(iter, env, this);
                    env.push(HashMap::new());
                    for n in names {
                        env.last_mut().unwrap().insert(n.clone(), it.clone());
                    }
                    self.stmts(body, env, this);
                    env.pop();
                }
                StmtKind::Return(Some(e))
                | StmtKind::Expr(e)
                | StmtKind::Fail(e)
                | StmtKind::Emit(e)
                | StmtKind::Expect(e) => {
                    last = self.expr(e, env, this);
                }
                StmtKind::Require { cond, message } => {
                    self.expr(cond, env, this);
                    if let Some(m) = message {
                        self.expr(m, env, this);
                    }
                }
                StmtKind::Assign { target, value, .. } => {
                    self.expr(target, env, this);
                    self.expr(value, env, this);
                }
                StmtKind::Fn(_) => self.deps.opaque = true,
                _ => {}
            }
        }
        env.pop();
        last
    }

    fn expr(&mut self, e: &Expr, env: &mut Env, this: &Val) -> Val {
        if self.depth > 24 {
            self.deps.opaque = true;
            return Val::Other;
        }
        self.depth += 1;
        let v = self.expr_inner(e, env, this);
        self.depth -= 1;
        v
    }

    fn expr_inner(&mut self, e: &Expr, env: &mut Env, this: &Val) -> Val {
        match &e.kind {
            ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Error => Val::Other,
            ExprKind::FString(parts) => {
                for p in parts {
                    if let FPart::Expr(x, _) = p {
                        self.expr(x, env, this);
                    }
                }
                Val::Other
            }
            ExprKind::Name(n) => {
                if let Some(v) = lookup(env, n) {
                    return v;
                }
                if n == "me" || n == "current" {
                    self.volatile(format!("reads `{n}`"));
                    return Val::Other;
                }
                if let Some(t) = self.entity_of(this) {
                    if self
                        .m
                        .entity(&t)
                        .map_or(false, |e| self.m.find_field(e, n).is_some())
                    {
                        return self.access(this.clone(), n);
                    }
                    if VOLATILE_MEMBERS.contains(&n.as_str()) {
                        self.volatile(format!("reads `{n}`"));
                        return Val::Other;
                    }
                }
                if self.m.entity(n).is_some() {
                    return Val::Table(n.clone());
                }
                if let Some(c) = self.m.constant(n) {
                    let c = c.value.clone();
                    return self.expr(&c, &mut vec![HashMap::new()], &Val::Other);
                }
                // a variant; anything else is looked up at run time (a currency or unit by its
                // code, a role)
                if self.m.variant(n).is_none() && !IMMUTABLE_MEMBERS.contains(&n.as_str()) {
                    self.volatile(format!("reads `{n}` by name"));
                }
                Val::Other
            }
            ExprKind::SelfRef => this.clone(),
            ExprKind::Field { base, name, .. } => {
                if name == "is_provisional" {
                    self.volatile("reads `is_provisional`".into());
                }
                let b = self.expr(base, env, this);
                self.access(b, name)
            }
            ExprKind::Call { callee, args } => self.call(callee, args, env, this),
            ExprKind::Index { base, index } => {
                let b = self.expr(base, env, this);
                self.expr(index, env, this);
                b
            }
            ExprKind::Unary { expr, .. } => {
                self.expr(expr, env, this);
                Val::Other
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.expr(lhs, env, this);
                let r = self.expr(rhs, env, this);
                match op {
                    // `a ?? b`: either side
                    BinOp::Coalesce => merge(self, vec![l, r]),
                    _ => Val::Other,
                }
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond, env, this);
                let a = self.expr(then, env, this);
                let b = self.expr(otherwise, env, this);
                merge(self, vec![a, b])
            }
            ExprKind::Lambda { params, body } => {
                env.push(params.iter().map(|p| (p.clone(), Val::Other)).collect());
                self.expr(body, env, this);
                env.pop();
                Val::Other
            }
            ExprKind::Tuple(items) => {
                let fields: Vec<(Option<String>, Val)> = items
                    .iter()
                    .map(|(n, x)| (n.clone(), self.expr(x, env, this)))
                    .collect();
                if fields.iter().all(|(_, v)| *v == Val::Other) {
                    Val::Other
                } else {
                    Val::Tuple(fields)
                }
            }
            ExprKind::List(items) => {
                let vs: Vec<Val> = items
                    .iter()
                    .map(|a| self.expr(&a.value, env, this))
                    .collect();
                merge(self, vs)
            }
            ExprKind::Set(items) => {
                let vs: Vec<Val> = items.iter().map(|x| self.expr(x, env, this)).collect();
                merge(self, vs)
            }
            ExprKind::When(arms) => {
                let mut vs = Vec::new();
                for (c, v) in arms {
                    if let Some(c) = c {
                        self.expr(c, env, this);
                    }
                    vs.push(self.expr(v, env, this));
                }
                merge(self, vs)
            }
            ExprKind::Match { subject, arms } => {
                let s = self.expr(subject, env, this);
                let mut vs = Vec::new();
                for arm in arms {
                    let mut scope = HashMap::new();
                    for p in &arm.patterns {
                        bind_pattern(self.m, p, &s, &mut scope);
                    }
                    env.push(scope);
                    vs.push(self.expr(&arm.body, env, this));
                    env.pop();
                }
                merge(self, vs)
            }
            ExprKind::Block(stmts) => self.stmts(stmts, env, this),
            ExprKind::As { body, .. } => {
                self.volatile("runs as another user".into());
                self.expr(body, env, this)
            }
            ExprKind::Fail(x) => {
                self.expr(x, env, this);
                Val::Other
            }
        }
    }

    /// `base.name`
    fn access(&mut self, base: Val, name: &str) -> Val {
        let (path, t, global) = match base {
            // a named field, or a positional one (`t.0`)
            Val::Tuple(fields) => {
                let named = fields
                    .iter()
                    .find(|(n, _)| n.as_deref() == Some(name))
                    .map(|(_, v)| v.clone());
                let positional = || {
                    name.parse::<usize>()
                        .ok()
                        .and_then(|i| fields.get(i))
                        .map(|(_, v)| v.clone())
                };
                return named.or_else(positional).unwrap_or(Val::Other);
            }
            Val::Path(p, t) => (p, t, false),
            Val::Global(t) => (Vec::new(), t, true),
            Val::Table(t) => {
                self.deps.globals.insert(t.clone());
                (Vec::new(), t, true)
            }
            Val::Other => return Val::Other,
        };
        let Some(e) = self.m.entity(&t) else {
            return Val::Other;
        };
        let Some(f) = self.m.find_field(e, name) else {
            if VOLATILE_MEMBERS.contains(&name) {
                self.volatile(format!("reads `{name}`"));
            }
            return Val::Other;
        };
        if f.per_company {
            self.volatile(format!("reads the per-company field `{t}.{name}`"));
        }
        if global {
            self.deps.globals.insert(t.clone());
        }
        let mk = |p: Vec<Step>, ent: String| {
            if global {
                Val::Global(ent)
            } else {
                Val::Path(p, ent)
            }
        };
        if let Some(formula) = &f.formula {
            let key = (t.clone(), name.to_string());
            if self.stack.contains(&key) {
                // the root formula read again further along the tree: a recursive step, when
                // its value is a plain value (what is read through a record it returns could
                // not be followed); anything else the analysis cannot follow
                let plain = f.ty.as_ref().is_some_and(|ty| {
                    ty_names(Some(ty)).map_or(!matches!(ty, Ty::List(_) | Ty::Set(_)), |n| {
                        self.m.entity(&n).is_none()
                    })
                });
                if self.stack.first() == Some(&key) && !global && plain {
                    self.deps.recursive.insert(path);
                } else {
                    self.deps.opaque = true;
                }
                return Val::Other;
            }
            // the value of a formula: what the formula reads, from the same record
            let formula = formula.clone();
            let inner = mk(path.clone(), t.clone());
            self.stack.push(key);
            let v = self.expr(&formula, &mut vec![HashMap::new()], &inner);
            self.stack.pop();
            return v;
        }
        let target = match &f.ty {
            Some(Ty::List(i)) | Some(Ty::Set(i)) => ty_names(Some(i)).map(|n| (n, true)),
            other => ty_names(other.as_ref()).map(|n| (n, false)),
        };
        let target = target.filter(|(n, _)| {
            self.m
                .entity(n)
                .map_or(false, |x| x.kind != EntityKind::Record)
        });
        match target {
            Some((child, true)) if f.kind == FieldKind::Stored && self.is_inverse(e, f, &child) => {
                if let Some(through) = &f.through {
                    let Some(te) = self.m.entity(through) else {
                        return Val::Other;
                    };
                    let back = te
                        .fields
                        .iter()
                        .find(|x| ty_names(x.ty.as_ref()).as_deref() == Some(t.as_str()))
                        .map(|x| x.name.clone());
                    let fwd = te
                        .fields
                        .iter()
                        .find(|x| ty_names(x.ty.as_ref()).as_deref() == Some(child.as_str()))
                        .map(|x| x.name.clone());
                    let (Some(back), Some(fwd)) = (back, fwd) else {
                        return Val::Other;
                    };
                    let p1 = push(
                        &path,
                        Step::Inv {
                            child: through.clone(),
                            back: back.clone(),
                        },
                    );
                    self.read(&p1, through, &back, global);
                    self.read(&p1, through, &fwd, global);
                    return mk(
                        push(
                            &p1,
                            Step::Ref {
                                ent: through.clone(),
                                field: fwd,
                            },
                        ),
                        child,
                    );
                }
                let Some(back) = self.back_field(e, f, &child) else {
                    self.deps.globals.insert(child.clone());
                    return Val::Global(child);
                };
                let p1 = push(
                    &path,
                    Step::Inv {
                        child: child.clone(),
                        back: back.clone(),
                    },
                );
                self.read(&p1, &child, &back, global);
                mk(p1, child)
            }
            Some((u, _)) => {
                self.read(&path, &t, name, global);
                mk(
                    push(
                        &path,
                        Step::Ref {
                            ent: t.clone(),
                            field: name.to_string(),
                        },
                    ),
                    u,
                )
            }
            None => {
                self.read(&path, &t, name, global);
                Val::Other
            }
        }
    }

    fn volatile(&mut self, why: String) {
        self.deps.volatile.get_or_insert(why);
    }

    fn read(&mut self, path: &[Step], ent: &str, field: &str, global: bool) {
        if global {
            self.deps.globals.insert(ent.to_string());
        } else {
            self.deps
                .reads
                .insert((path.to_vec(), ent.to_string(), field.to_string()));
        }
    }

    /// A collection field computed from the other side (not stored on the record).
    fn is_inverse(&self, e: &Entity, f: &Field, child: &str) -> bool {
        f.through.is_some() || f.via.is_some() || self.back_field(e, f, child).is_some()
    }

    fn back_field(&self, owner: &Entity, f: &Field, child: &str) -> Option<String> {
        let ce = self.m.entity(child)?;
        f.via.clone().or_else(|| {
            ce.fields
                .iter()
                .find(|x| {
                    let n = match &x.ty {
                        Some(Ty::Set(i)) | Some(Ty::List(i)) => ty_names(Some(i)),
                        other => ty_names(other.as_ref()),
                    };
                    n.as_deref() == Some(owner.name.as_str())
                        || n.map_or(false, |n| owner.traits.contains(&n))
                })
                .map(|x| x.name.clone())
        })
    }

    fn call(&mut self, callee: &Expr, args: &[Arg], env: &mut Env, this: &Val) -> Val {
        match &callee.kind {
            ExprKind::Field { base, name, .. } => {
                let b = self.expr(base, env, this);
                self.method(b, name, args, env, this)
            }
            ExprKind::Name(n) => {
                match n.as_str() {
                    "today" | "now" => {
                        self.deps.time = true;
                        return Val::Other;
                    }
                    "convert" => {
                        self.deps.globals.insert("ExchangeRate".into());
                    }
                    "next_number" | "why" | "login" | "affected" | "add" => {
                        self.volatile(format!("calls `{n}()`"));
                    }
                    _ => {}
                }
                if self.m.entity(n).is_some() {
                    self.volatile(format!("creates a {n}"));
                }
                let vals: Vec<Val> = args
                    .iter()
                    .map(|a| self.expr(&a.value, env, this))
                    .collect();
                if self.m.metrics.iter().any(|x| &x.name == n) {
                    self.deps.opaque = true;
                    return Val::Other;
                }
                if let Some(f) = self.m.function(n) {
                    let f = f.clone();
                    let Some(body) = &f.body else {
                        if f.builtin {
                            return Val::Other;
                        }
                        self.deps.opaque = true;
                        return Val::Other;
                    };
                    let mut scope = HashMap::new();
                    for (i, p) in f.params.iter().enumerate() {
                        let named = args
                            .iter()
                            .position(|a| a.name.as_deref() == Some(p.name.as_str()));
                        let v = named.and_then(|j| vals.get(j).cloned()).or_else(|| {
                            args.iter()
                                .position(|a| a.name.is_none())
                                .and(vals.get(i).cloned())
                        });
                        let v = match (v, &p.default) {
                            (Some(v), _) => v,
                            (None, Some(d)) => self.expr(d, env, this),
                            (None, None) => Val::Other,
                        };
                        scope.insert(p.name.clone(), v);
                    }
                    let mut fenv: Env = vec![scope];
                    self.stmts(body, &mut fenv, &Val::Other);
                    // a function returning a record: anything about it is global
                    if let Some(rt) = ty_names(f.ret.as_ref()).filter(|n| {
                        self.m
                            .entity(n)
                            .map_or(false, |e| e.kind != EntityKind::Record)
                    }) {
                        self.deps.globals.insert(rt.clone());
                        return Val::Global(rt);
                    }
                    return Val::Other;
                }
                Val::Other
            }
            _ => {
                self.expr(callee, env, this);
                for a in args {
                    self.expr(&a.value, env, this);
                }
                Val::Other
            }
        }
    }

    fn method(&mut self, base: Val, name: &str, args: &[Arg], env: &mut Env, this: &Val) -> Val {
        // the element seen by a lambda over `base`
        let elem = match &base {
            Val::Table(t) => self.table_element(t, args, env, this),
            other => other.clone(),
        };
        let mut body_val = Val::Other;
        let mut key_val = Val::Other;
        for a in args {
            match &a.value.kind {
                ExprKind::Lambda { params, body } => {
                    let mut scope = HashMap::new();
                    if let Some(p) = params.first() {
                        scope.insert(p.clone(), elem.clone());
                    }
                    for p in params.iter().skip(1) {
                        scope.insert(p.clone(), elem.clone());
                    }
                    env.push(scope);
                    body_val = self.expr(body, env, this);
                    env.pop();
                    if a.name.is_none() && key_val == Val::Other {
                        key_val = body_val.clone();
                    }
                }
                _ => {
                    self.expr(&a.value, env, this);
                }
            }
        }
        // methods defined on the entity
        if let Some(t) = self.entity_of(&elem) {
            if let Some(me) = self
                .m
                .entity(&t)
                .and_then(|e| e.methods.iter().find(|x| x.name == name))
                .cloned()
            {
                if let Some(body) = &me.body {
                    let mut fenv: Env = vec![HashMap::new()];
                    for p in &me.params {
                        fenv[0].insert(p.name.clone(), Val::Other);
                    }
                    return self.stmts(body, &mut fenv, &elem);
                }
            }
        }
        match name {
            "where" | "filter" | "sort_by" | "sorted" | "first" | "last" | "last_by" | "find"
            | "single" | "min_by" | "max_by" | "distinct" | "all" | "take" | "skip" | "reverse"
            | "unique" => match elem {
                Val::Other => Val::Other,
                v => v,
            },
            // the groups: `key` is what the lambda returns, `items` the elements
            "group_by" => match (&key_val, &elem) {
                (Val::Other, Val::Other) => Val::Other,
                _ => Val::Tuple(vec![
                    (Some("key".into()), key_val),
                    (Some("items".into()), elem),
                ]),
            },
            "map" | "flat_map" | "max" | "min" => body_val,
            _ => Val::Other,
        }
    }

    /// `Table.where(x => x.col == <path>)`: the rows reached from the root through `col`.
    fn table_element(&mut self, t: &str, args: &[Arg], env: &mut Env, this: &Val) -> Val {
        if let Some(Arg {
            value:
                Expr {
                    kind: ExprKind::Lambda { params, body },
                    ..
                },
            ..
        }) = args.first()
        {
            if let Some(p) = params.first() {
                for conj in conjuncts(body) {
                    let ExprKind::Binary {
                        op: BinOp::Eq,
                        lhs,
                        rhs,
                    } = &conj.kind
                    else {
                        continue;
                    };
                    for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                        let ExprKind::Field {
                            base, name: col, ..
                        } = &side.kind
                        else {
                            continue;
                        };
                        if !matches!(&base.kind, ExprKind::Name(n) if n == p) {
                            continue;
                        }
                        let ov = self.expr(other, env, this);
                        if let Val::Path(q, _) = ov {
                            let p1 = push(
                                &q,
                                Step::Inv {
                                    child: t.to_string(),
                                    back: col.clone(),
                                },
                            );
                            self.deps
                                .reads
                                .insert((p1.clone(), t.to_string(), col.clone()));
                            return Val::Path(p1, t.to_string());
                        }
                    }
                }
            }
        }
        self.deps.globals.insert(t.to_string());
        Val::Global(t.to_string())
    }
}

fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match &e.kind {
        ExprKind::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } => {
            let mut v = conjuncts(lhs);
            v.extend(conjuncts(rhs));
            v
        }
        _ => vec![e],
    }
}

fn bind_pattern(m: &Model, p: &Pattern, v: &Val, scope: &mut HashMap<String, Val>) {
    match p {
        // lower-case names bind the subject (upper-case ones are variants and types)
        Pattern::Name(n) if n.chars().next().map_or(false, |c| c.is_lowercase()) => {
            scope.insert(n.clone(), v.clone());
        }
        Pattern::Ctor { name, args } => {
            // `PurchaseLine(pl)`: a type test on a reference; `pl` is the subject itself
            let entity = m
                .entity(name)
                .map_or(false, |e| e.kind != EntityKind::Record);
            for a in args {
                let inner = match (entity, v) {
                    (true, Val::Path(p, _)) => Val::Path(p.clone(), name.clone()),
                    (true, Val::Global(_)) => Val::Global(name.clone()),
                    _ => Val::Other,
                };
                bind_pattern(m, a, &inner, scope);
            }
        }
        _ => {}
    }
}

/// One value for several alternatives: the same path, or global.
fn merge(a: &mut An, vs: Vec<Val>) -> Val {
    let mut out: Option<Val> = None;
    for v in vs {
        match (&out, v) {
            (_, Val::Other) => {}
            (None, v) => out = Some(v),
            (Some(Val::Path(p, t)), Val::Path(q, u)) if *p == q && *t == u => {}
            (Some(prev), v) if *prev == v => {}
            // tuples of the same shape: field by field
            (Some(Val::Tuple(a_fields)), Val::Tuple(b_fields))
                if a_fields.len() == b_fields.len()
                    && a_fields.iter().zip(&b_fields).all(|(x, y)| x.0 == y.0) =>
            {
                let fields = a_fields
                    .clone()
                    .into_iter()
                    .zip(b_fields)
                    .map(|((n, x), (_, y))| (n, merge(a, vec![x, y])))
                    .collect();
                out = Some(Val::Tuple(fields));
            }
            // a tuple and something else: the analysis cannot follow it
            (Some(Val::Tuple(_)), _) | (Some(_), Val::Tuple(_)) => {
                a.deps.opaque = true;
                out = Some(Val::Other);
            }
            (Some(prev), v) => {
                for t in [a.entity_of(prev), a.entity_of(&v)].into_iter().flatten() {
                    a.deps.globals.insert(t);
                }
                let t = a.entity_of(&v).unwrap_or_default();
                out = Some(Val::Global(t));
            }
        }
    }
    out.unwrap_or(Val::Other)
}

/// The entity a type names: `Partner`, `Partner?` (not lists).
pub fn ty_names(t: Option<&Ty>) -> Option<String> {
    match t? {
        Ty::Path { segments, .. } if segments.len() == 1 => Some(segments[0].clone()),
        Ty::Optional(i) => ty_names(Some(i)),
        _ => None,
    }
}
