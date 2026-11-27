//! The checker proper. See lib.rs for what it proves.

use crate::ty::{Fact, Num, Unit, T};
use quadra_ir::*;
use quadra_sema::{Diagnostic, Severity};
use std::collections::{HashMap, HashSet};

pub fn check_model(model: &Model) -> Vec<Diagnostic> {
    analyze_model(model).diagnostics
}

/// A field of an entity or ledger (`("Invoice", "total")`), or `("metric", name)`.
pub type Node = (String, String);

/// Everything the checker learned besides errors: who reads and who writes each field.
#[derive(Default, Debug)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
    /// formula / ledger column / metric → the fields it reads
    pub reads: HashMap<Node, Vec<(Node, Span)>>,
    /// field → places that assign it (`action Invoice.post`, `reaction …`)
    pub writes: HashMap<Node, Vec<(String, Span)>>,
    /// type of every field and formula, relative to its own record
    pub field_types: HashMap<Node, T>,
    /// type of every `sum(…)` of amounts, quantities or prices, by the span of the call:
    /// the zero of an empty sum (units are paths in the scope of the call)
    pub sum_types: HashMap<Span, T>,
    /// every member of a record named in the code (`o.total`, `o.confirm()`), by the span of
    /// its name: find references and rename in the editor
    pub accesses: Vec<(Span, Node)>,
    /// the record of the rows of every `rows.csv()` and `rows.fixed()`, by the span of the
    /// method: the header of an empty list (`Model::row_records`)
    pub row_records: HashMap<Span, String>,
}

pub fn analyze_model(model: &Model) -> Report {
    let mut c = Checker::new(model);
    c.run();
    let mut field_types = HashMap::new();
    for e in &model.entities {
        for f in &e.fields {
            let t = if let Some(ty) = &f.ty {
                c.entity = Some(e);
                let t = c.decl_ty_field(ty, f.per_company);
                c.entity = None;
                t
            } else if f.formula.is_some() {
                c.formula_type(e, f)
            } else {
                continue;
            };
            field_types.insert((e.name.clone(), f.name.clone()), t);
        }
    }
    c.diags.sort_by_key(|d| (d.file, d.range.start()));
    c.diags
        .dedup_by(|a, b| a.file == b.file && a.range == b.range && a.message == b.message);
    for v in c.reads.values_mut() {
        v.dedup_by(|a, b| a.0 == b.0);
    }
    c.accesses.sort_by_key(|(s, _)| (s.file, s.range.start()));
    c.accesses.dedup();
    Report {
        diagnostics: c.diags,
        reads: c.reads,
        writes: c.writes,
        field_types,
        sum_types: c.sum_types,
        row_records: c.row_records,
        accesses: c.accesses,
    }
}

#[derive(Clone)]
struct Local {
    ty: T,
    /// Locally defined function (`fn line(...)` inside a reaction).
    func: Option<FnDef>,
}

struct Checker<'m> {
    m: &'m Model,
    diags: Vec<Diagnostic>,
    /// unit code → category
    units: HashMap<String, String>,
    currencies: HashSet<String>,
    scopes: Vec<HashMap<String, Local>>,
    /// collection facts for lambda parameters
    facts: HashMap<String, Vec<Fact>>,
    entity: Option<&'m Entity>,
    /// inside a `per company` member: bare `company` is the company being computed
    company_ctx: bool,
    in_formula: bool,
    in_fstring: usize,
    narrowed: Vec<String>,
    fn_ret: Option<T>,
    group_key: Option<T>,
    fixture_scopes: HashMap<String, HashMap<String, Local>>,
    /// formula types being inferred (cycle guard) and cache
    formula_cache: HashMap<(String, String), T>,
    in_progress: HashSet<(String, String)>,
    /// node whose dependencies are being recorded
    current: Option<Node>,
    /// description of the code being checked, for `writes`
    context: String,
    reads: HashMap<Node, Vec<(Node, Span)>>,
    writes: HashMap<Node, Vec<(String, Span)>>,
    sum_types: HashMap<Span, T>,
    accesses: Vec<(Span, Node)>,
    /// `rows.csv()`, `rows.fixed()` → the record of the rows (`Report::row_records`)
    row_records: HashMap<Span, String>,
}

/// The statements that write (an assignment, an `emit`), in a block and the blocks in it.
fn writes(stmts: &[Stmt], out: &mut Vec<Span>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Assign { .. } | StmtKind::Emit(_) => out.push(s.span),
            StmtKind::If {
                then, otherwise, ..
            } => {
                writes(then, out);
                if let Some(e) = otherwise {
                    writes(e, out);
                }
            }
            StmtKind::For { body, .. } => writes(body, out),
            _ => {}
        }
    }
}

const TEXT_TYPES: &[&str] = &[
    "Text",
    "Code",
    "Email",
    "Phone",
    "VatNumber",
    "Barcode",
    "File",
];

fn path_str(p: &[String]) -> String {
    p.join(".")
}

impl<'m> Checker<'m> {
    fn new(m: &'m Model) -> Checker<'m> {
        let mut c = Checker {
            m,
            diags: Vec::new(),
            units: HashMap::new(),
            currencies: HashSet::new(),
            scopes: vec![Default::default()],
            facts: HashMap::new(),
            entity: None,
            company_ctx: false,
            in_formula: false,
            in_fstring: 0,
            narrowed: Vec::new(),
            fn_ret: None,
            group_key: None,
            fixture_scopes: HashMap::new(),
            formula_cache: HashMap::new(),
            in_progress: HashSet::new(),
            current: None,
            context: String::new(),
            reads: HashMap::new(),
            writes: HashMap::new(),
            sum_types: HashMap::new(),
            accesses: Vec::new(),
            row_records: HashMap::new(),
        };
        c.collect_units();
        c
    }

    /// `name` of a record of type `base` written at the end of `span`.
    fn record_access(&mut self, base: &T, name: &str, span: Span) {
        let T::Ent(en) = base else { return };
        let Some(e) = self.m.entity(en) else { return };
        let known = self.m.find_field(e, name).is_some()
            || e.actions.iter().any(|a| a.name == name)
            || e.methods.iter().any(|f| f.name == name)
            || self.is_flow_action(e, name);
        let end = span.range.end();
        let len = quadra_syntax::TextSize::from(name.len() as u32);
        if known && end >= span.range.start() + len {
            let range = quadra_syntax::TextRange::new(end - len, end);
            self.accesses.push((
                Span {
                    file: span.file,
                    range,
                },
                (en.clone(), name.to_string()),
            ));
        }
    }

    fn record_read(&mut self, entity: &str, field: &str, span: Span) {
        if let Some(cur) = self.current.clone() {
            let node = (entity.to_string(), field.to_string());
            if self
                .reads
                .get(&cur)
                .map_or(true, |v| !v.iter().any(|(n, _)| *n == node))
            {
                self.reads.entry(cur).or_default().push((node, span));
            }
        }
    }

    fn record_write(&mut self, entity: &str, field: &str, span: Span) {
        let ctx = self.context.clone();
        self.writes
            .entry((entity.to_string(), field.to_string()))
            .or_default()
            .push((ctx, span));
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic {
            file: span.file,
            range: span.range,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    // ---- seeds: units and currencies ------------------------------------------------------

    fn collect_units(&mut self) {
        for seed in &self.m.seeds {
            let mut cats: HashMap<String, String> = HashMap::new();
            for s in &seed.body {
                match &s.kind {
                    StmtKind::Let { name, value, .. } => {
                        if let Some((callee, args)) = call_parts(value) {
                            if callee == "UomCategory" {
                                if let Some(n) = str_arg(args, "name") {
                                    cats.insert(name.clone(), n);
                                }
                            }
                        }
                    }
                    StmtKind::Expr(e) => {
                        if let Some((callee, args)) = call_parts(e) {
                            let code = str_arg(args, "code");
                            match (callee, code) {
                                ("Uom", Some(code)) => {
                                    let cat = args
                                        .iter()
                                        .find(|a| a.name.as_deref() == Some("category"))
                                        .and_then(|a| match &a.value.kind {
                                            ExprKind::Name(v) => cats.get(v).cloned(),
                                            _ => None,
                                        })
                                        .unwrap_or_default();
                                    self.units.insert(code, cat);
                                }
                                ("Currency", Some(code)) => {
                                    self.currencies.insert(code);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn is_duration_unit(u: &str) -> bool {
        crate::duration_unit(u).is_some()
    }

    // ---- driver -----------------------------------------------------------------------

    fn run(&mut self) {
        let m = self.m;
        for (span, message) in &m.errors {
            self.error(*span, message.clone());
        }
        for e in &m.entities {
            self.check_entity(e);
        }
        for f in &m.fns {
            self.entity = None;
            self.context = format!("fn {}", f.name);
            self.check_fn(f);
        }
        for c in &m.consts {
            self.context = format!("const {}", c.name);
            let t = self.infer(&c.value);
            if let Some(ty) = &c.ty {
                let d = self.decl_ty(ty, &["self".into()]);
                self.assign(&d, &t, c.value.span, "constant");
            }
        }
        self.context = String::new();
        self.check_outbounds();
        self.check_inbounds();
        self.check_endpoints();
        self.context = String::new();
        for r in &m.reactions {
            self.entity = None;
            self.context = match &r.trigger {
                Trigger::Enters(s) => format!("on {} enters {s}", r.entity),
                Trigger::Becomes(e) => format!("on {} becomes {}", r.entity, print::expr(e)),
                Trigger::Before(a) => format!("before {}.{a}", r.entity),
                Trigger::Created => format!("on {} created", r.entity),
                Trigger::Changes => format!("on {} changes", r.entity),
                Trigger::Delivered => format!("on {} delivered", r.entity),
                Trigger::Failed => format!("on {} failed", r.entity),
            };
            self.push();
            if matches!(r.trigger, Trigger::Delivered | Trigger::Failed) {
                let t = self.outcome_type(r);
                self.bind(&r.binding, t);
                self.stmts(&r.body);
                self.pop();
                continue;
            }
            self.bind(&r.binding, T::Ent(r.entity.clone()));
            if let Trigger::Becomes(e) = &r.trigger {
                self.entity = m.entity(&r.entity);
                let saved = self.scopes.clone();
                self.infer(e);
                self.scopes = saved;
                self.entity = None;
            }
            self.stmts(&r.body);
            self.pop();
        }
        for role in &m.roles {
            for rule in &role.rules {
                self.check_rule(rule);
            }
        }
        for rule in &m.rules {
            self.check_rule(rule);
        }
        self.context = "seed".into();
        for s in &m.seeds {
            self.push();
            self.stmts(&s.body);
            self.pop();
        }
        // fixtures in dependency order, remembering their variables for `use`
        let mut done: HashSet<String> = HashSet::new();
        let mut pending: Vec<&Block> = m.fixtures.iter().collect();
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|fx| {
                let ready = fx.extends.as_ref().map_or(true, |p| {
                    done.contains(p) || !m.fixtures.iter().any(|f| &f.name == p)
                });
                if !ready {
                    return true;
                }
                self.context = format!("fixture {}", fx.name);
                let mut scope = fx
                    .extends
                    .as_ref()
                    .and_then(|p| self.fixture_scopes.get(p).cloned())
                    .unwrap_or_default();
                self.scopes.push(std::mem::take(&mut scope));
                self.stmts(&fx.body);
                let s = self.scopes.pop().unwrap();
                self.fixture_scopes.insert(fx.name.clone(), s);
                done.insert(fx.name.clone());
                false
            });
            if pending.len() == before {
                break;
            }
        }
        for t in &m.tests {
            self.context = format!("test {:?}", t.name);
            self.push();
            self.stmts(&t.body);
            self.pop();
        }
        for metric in &m.metrics {
            self.push();
            for p in &metric.params {
                let t =
                    p.ty.as_ref()
                        .map(|t| self.decl_ty(t, &[p.name.clone()]))
                        .unwrap_or(T::Unknown);
                self.bind(&p.name, self.param_unit_root(&p.name, t));
            }
            for (_, e) in &metric.meta {
                self.infer(e);
            }
            let saved = self.in_formula;
            self.in_formula = !metric.control;
            self.current = Some(("metric".into(), metric.name.clone()));
            for (_, e) in &metric.formulas {
                let t = self.infer(e);
                if let Some(ret) = &metric.ret {
                    let d = self.decl_ty(ret, &["self".into()]);
                    self.assign(&d, &t, e.span, "metric");
                }
            }
            self.in_formula = saved;
            self.current = None;
            self.pop();
        }
        for mig in m.migrations.iter().chain(m.changes.iter()) {
            self.context = format!("migration {}", mig.name);
            for s in &mig.body {
                if let StmtKind::Migration { keyword, paths, .. } = &s.kind {
                    if let Some(p) = paths.last().filter(|p| p.len() == 2 && keyword != "rename") {
                        self.record_write(&p[0], &p[1], s.span);
                    }
                }
            }
        }
    }

    fn check_rule(&mut self, rule: &Rule) {
        let Some(cond) = &rule.condition else { return };
        let target = rule
            .targets
            .first()
            .and_then(|t| t.first())
            .cloned()
            .unwrap_or_default();
        self.push();
        if let ExprKind::Lambda { params, body } = &cond.kind {
            if let Some(p) = params.first() {
                let t = if self.m.entity(&target).is_some() {
                    T::Ent(target)
                } else {
                    T::Unknown
                };
                self.bind(p, t);
            }
            self.infer(body);
        } else {
            self.infer(cond);
        }
        self.pop();
    }

    fn check_entity(&mut self, e: &'m Entity) {
        self.check_record_format(e);
        self.entity = Some(e);
        let self_root = vec!["self".to_string()];
        for f in &e.fields {
            let decl = f.ty.as_ref().map(|t| self.decl_ty_field(t, f.per_company));
            self.company_ctx = f.per_company;
            if let Some(d) = &f.default {
                self.in_formula = false;
                self.current = Some((e.name.clone(), format!("{}#default", f.name)));
                let t = self.infer(d);
                self.current = None;
                if let Some(decl) = &decl {
                    self.assign(decl, &t, d.span, "default");
                }
            }
            if let Some(formula) = &f.formula {
                self.in_formula = true;
                self.current = Some((e.name.clone(), f.name.clone()));
                let t = self.infer(formula);
                self.current = None;
                if let Some(decl) = &decl {
                    self.assign(decl, &t, formula.span, "formula");
                }
                self.in_formula = false;
            }
            if let Some(w) = &f.where_ {
                let ft = decl.clone().unwrap_or(T::Unknown);
                match w {
                    Where::Compare(op, x) => {
                        let xt = self.infer(x);
                        self.binary_types(*op, ft.unopt().clone(), xt, x.span);
                    }
                    Where::Expr(x) => {
                        if let ExprKind::Lambda { params, body } = &x.kind {
                            self.push();
                            if let Some(p) = params.first() {
                                self.bind(p, ft.unopt().clone());
                            }
                            self.infer(body);
                            self.pop();
                        } else {
                            self.infer(x);
                        }
                    }
                }
            }
            if let Some(r) = &f.required_when {
                self.infer(r);
            }
            self.company_ctx = false;
        }
        let _ = self_root;
        for c in &e.checks {
            if e.is_ledger && c.warn {
                self.error(c.span, "a ledger row cannot carry a warning (rows are never edited): use `check` to refuse the row");
            }
            if !c.per.is_empty() {
                let key: Vec<(Option<String>, T)> = c
                    .per
                    .iter()
                    .map(|k| {
                        (
                            Some(k.clone()),
                            e.field(k)
                                .and_then(|f| f.ty.as_ref())
                                .map(|t| self.decl_ty(t, &["self".into()]))
                                .unwrap_or(T::Unknown),
                        )
                    })
                    .collect();
                self.group_key = Some(T::Tuple(key));
                self.push();
                if let ExprKind::Lambda { params, body } = &c.cond.kind {
                    if let Some(p) = params.first() {
                        // rows of a group share its key columns
                        let facts = c
                            .per
                            .iter()
                            .map(|k| Fact {
                                rel: vec![k.clone()],
                                target: vec![p.clone(), "key".into(), k.clone()],
                            })
                            .collect();
                        self.bind(
                            p,
                            T::Coll {
                                elem: Box::new(T::Ent(e.name.clone())),
                                set: false,
                                facts,
                            },
                        );
                    }
                    self.infer(body);
                } else {
                    self.infer(&c.cond);
                }
                self.pop();
                self.group_key = None;
            } else {
                self.infer(&c.cond);
            }
            self.in_fstring += 1;
            self.infer(&c.message);
            self.in_fstring -= 1;
        }
        for u in &e.uniques {
            if let Some(w) = &u.when {
                self.infer(w);
            }
        }
        if let Some(fr) = &e.frozen {
            self.infer(&fr.when);
        }
        // `except` / `live`: fields of this entity; `live` only where something is frozen (the
        // entity, or the document it is a line of)
        let m = self.m;
        let has = |n: &str| m.find_field(e, n).is_some();
        if let Some(fr) = &e.frozen {
            for n in fr.except.iter().chain(
                fr.live
                    .iter()
                    .filter(|n| !e.live_decls.iter().any(|(d, _)| d == *n)),
            ) {
                if !has(n) {
                    self.error(fr.span, format!("`{n}` is not a field of {}", e.name));
                }
            }
        }
        for (n, span) in &e.live_decls {
            if m.frozen_owner(e).is_none() {
                self.error(*span, format!("`live {n}`: {} is never frozen (it has no `frozen when`, and it is not a line of a document that has one)", e.name));
            } else if !has(n) {
                self.error(*span, format!("`{n}` is not a field of {}", e.name));
            } else if m.find_field(e, n).map_or(false, |f| f.formula.is_none()) {
                self.error(*span, format!("`live {n}`: only formulas can be live (`{n}` is stored; to change it on a frozen record use `except`)"));
            }
        }
        for flow in &e.flows {
            for t in &flow.transitions {
                if let Some(w) = &t.when {
                    self.infer(w);
                }
                if let Some(u) = &t.unless {
                    self.infer(u);
                }
            }
        }
        for flow in &e.flows {
            for t in &flow.transitions {
                self.context = format!(
                    "flow {} → {}{}",
                    t.from.join(" | "),
                    t.to,
                    t.by.as_ref()
                        .map(|b| format!(" by {b}"))
                        .unwrap_or_default()
                );
                self.record_write(&e.name, &flow.field, t.span);
            }
        }
        for a in &e.actions {
            self.context = format!("action {}.{}", e.name, a.name);
            self.push();
            self.bind_params(&a.params);
            self.fn_ret = a.ret.as_ref().map(|t| self.decl_ty(t, &["self".into()]));
            self.stmts(&a.body);
            self.fn_ret = None;
            self.pop();
        }
        for f in &e.methods {
            self.context = format!("fn {}.{}", e.name, f.name);
            self.check_fn(f);
        }
        for p in &e.posts {
            self.context = format!("post {} → {}", e.name, p.ledger);
            if let Some(w) = &p.when {
                self.current = Some((p.ledger.clone(), "*".into()));
                self.infer(w);
                self.current = None;
            }
            self.push();
            self.bind("__ledger", T::Ent(p.ledger.clone()));
            self.stmts(&p.body);
            self.pop();
        }
        self.entity = None;
    }

    fn check_fn(&mut self, f: &FnDef) {
        let Some(body) = &f.body else { return };
        self.push();
        self.bind_params(&f.params);
        self.fn_ret = f.ret.as_ref().map(|t| self.decl_ty(t, &["self".into()]));
        self.stmts(body);
        self.fn_ret = None;
        self.pop();
    }

    fn bind_params(&mut self, params: &[Param]) {
        for p in params {
            if let Some(d) = &p.default {
                self.infer(d);
            }
            let t =
                p.ty.as_ref()
                    .map(|t| self.decl_ty(t, &["self".into()]))
                    .unwrap_or(T::Unknown);
            let t = self.param_unit_root(&p.name, t);
            self.bind(&p.name, t);
        }
    }

    /// An unparameterized `Money` parameter has its own currency: `Money<amount.currency>`.
    fn param_unit_root(&self, name: &str, t: T) -> T {
        match t {
            T::Money {
                cur: Unit::Unknown,
                exact,
            } => T::Money {
                cur: Unit::Path(vec![name.into(), "currency".into()]),
                exact,
            },
            other => other,
        }
    }

    // ---- scopes -----------------------------------------------------------------------

    fn push(&mut self) {
        self.scopes.push(Default::default());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str, ty: T) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.to_string(), Local { ty, func: None });
        }
    }

    fn local(&self, name: &str) -> Option<&Local> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    // ---- declared types ---------------------------------------------------------------

    fn decl_ty_field(&mut self, ty: &Ty, per_company: bool) -> T {
        let saved = self.company_ctx;
        self.company_ctx = per_company;
        let t = self.decl_ty(ty, &["self".into()]);
        self.company_ctx = saved;
        t
    }

    /// Type of a declaration. Unit paths are rooted at `root` when they start with a field
    /// of the current entity, at a parameter or local, at `current`, or at `company` in a
    /// per-company member.
    fn decl_ty(&mut self, ty: &Ty, root: &[String]) -> T {
        match ty {
            Ty::Path {
                segments,
                args,
                span,
            } => {
                if segments.len() != 1 {
                    return T::Unknown;
                }
                let name = segments[0].as_str();
                let unit = |c: &mut Self, i: usize| {
                    args.get(i)
                        .map(|a| c.unit_of_ty(a, root))
                        .unwrap_or(Unit::Unknown)
                };
                match name {
                    n if TEXT_TYPES.contains(&n) => {
                        T::Text(TEXT_TYPES.iter().find(|t| **t == n).unwrap())
                    }
                    "Int" => T::Num(Num::Int),
                    "Decimal" => T::Num(Num::Decimal),
                    "Percent" => T::Num(Num::Percent),
                    "Bool" => T::Bool,
                    "Date" => T::Date,
                    "DateTime" => T::DateTime,
                    "Duration" => T::Duration,
                    "DateRange" => T::DateRange,
                    "Role" => T::Role,
                    "Money" => T::Money {
                        cur: unit(self, 0),
                        exact: false,
                    },
                    "Price" => T::Price {
                        cur: unit(self, 0),
                        per: unit(self, 1),
                        exact: false,
                    },
                    "Qty" => T::Qty {
                        unit: unit(self, 0),
                        exact: false,
                    },
                    "Exact" => args
                        .first()
                        .map(|a| self.decl_ty(a, root).with_exact(true))
                        .unwrap_or(T::Unknown),
                    n if self.m.entity(n).is_some() => T::Ent(n.to_string()),
                    n => match self.m.enum_def(n) {
                        Some(e) => T::Enum {
                            name: n.to_string(),
                            variants: e.variants.iter().map(|v| v.name.clone()).collect(),
                        },
                        None => {
                            let _ = span;
                            T::Unknown
                        }
                    },
                }
            }
            Ty::Optional(i) => T::opt(self.decl_ty(i, root)),
            Ty::List(i) => T::list(self.decl_ty(i, root)),
            Ty::Set(i) => T::Coll {
                elem: Box::new(self.decl_ty(i, root)),
                set: true,
                facts: Vec::new(),
            },
            Ty::Tuple(fs) => T::Tuple(
                fs.iter()
                    .map(|(n, t)| (Some(n.clone()), self.decl_ty(t, root)))
                    .collect(),
            ),
            Ty::Enum(vs) => T::Enum {
                name: format!(
                    "enum({})",
                    vs.iter()
                        .map(|v| v.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                variants: vs.iter().map(|v| v.name.clone()).collect(),
            },
            Ty::Union(_) | Ty::Error => T::Unknown,
        }
    }

    fn unit_of_ty(&mut self, ty: &Ty, root: &[String]) -> Unit {
        let Ty::Path { segments, span, .. } = ty else {
            return Unit::Unknown;
        };
        let first = segments[0].as_str();
        if segments.len() == 1 {
            let is_field = self
                .entity
                .map_or(false, |e| self.m.find_field(e, first).is_some());
            let is_local = self.local(first).is_some();
            if !is_field && !is_local && first != "company" {
                if !self.units.contains_key(first) && !self.currencies.contains(first) {
                    self.error(*span, format!("unknown unit or currency `{first}`"));
                }
                return Unit::Const(first.to_string());
            }
        }
        if first == "owner" || first == "current" {
            return Unit::Path(segments.clone());
        }
        if first == "company" && self.company_ctx {
            return Unit::Path(segments.clone());
        }
        if self.local(first).is_some() {
            return Unit::Path(segments.clone());
        }
        let mut p = root.to_vec();
        p.extend(segments.iter().cloned());
        Unit::Path(p)
    }

    // ---- assignability ----------------------------------------------------------------

    fn assign(&mut self, target: &T, value: &T, span: Span, what: &str) {
        let target_inner = target.unopt();
        let value_inner = value.unopt();
        match (target_inner, value_inner) {
            (
                T::Money {
                    cur: a,
                    exact: false,
                },
                T::Money { cur: b, exact },
            ) => {
                if *exact {
                    self.error(
                        span,
                        format!(
                            "{value} must be rounded with .round() before it is stored as {target}"
                        ),
                    );
                }
                if let (Unit::Const(x), Unit::Const(y)) = (a, b) {
                    if x != y {
                        self.error(
                            span,
                            format!("cannot store {value} in a {what} of type {target}"),
                        );
                    }
                }
            }
            (T::Price { exact: false, .. }, T::Price { exact: true, .. }) => {
                self.error(
                    span,
                    format!(
                        "{value} must be rounded with .round() before it is stored as {target}"
                    ),
                );
            }
            (
                T::Qty {
                    unit: Unit::Const(a),
                    ..
                },
                T::Qty {
                    unit: Unit::Const(b),
                    ..
                },
            ) => {
                if self.units.get(a) != self.units.get(b) {
                    self.error(
                        span,
                        format!("cannot store {value} in a {what} of type {target}"),
                    );
                }
            }
            (T::Ent(a), T::Ent(b)) => {
                if a != b && !self.compatible_entities(a, b) {
                    self.error(span, format!("expected {target}, found {value}"));
                }
            }
            (T::Enum { name, variants }, T::Variant(v)) => {
                if !variants.contains(v) {
                    self.error(span, format!("`{v}` is not a value of {name}"));
                }
            }
            // different kinds of value: an amount where a quantity is expected, text for a date…
            (t, v) => {
                if let (Some(a), Some(b)) = (kind_of(t), kind_of(v)) {
                    if a != b {
                        self.error(
                            span,
                            format!("cannot store {value} in a {what} of type {target}"),
                        );
                    }
                }
            }
        }
    }

    fn compatible_entities(&self, target: &str, value: &str) -> bool {
        let (Some(t), Some(v)) = (self.m.entity(target), self.m.entity(value)) else {
            return true;
        };
        self.m.implements(v, &t.name) || self.m.implements(t, &v.name)
    }

    // ---- statements -------------------------------------------------------------------

    fn stmts(&mut self, stmts: &[Stmt]) {
        let n = self.narrowed.len();
        for s in stmts {
            self.stmt(s);
            // `if x == none: return …` → x is not none afterwards
            if let StmtKind::If {
                cond,
                then,
                otherwise: None,
                binding: None,
            } = &s.kind
            {
                if then.last().map_or(false, |l| {
                    matches!(l.kind, StmtKind::Return(_) | StmtKind::Fail(_))
                }) {
                    let f = self.facts_false(cond);
                    self.narrowed.extend(f);
                }
            }
        }
        self.narrowed.truncate(n);
    }

    fn block(&mut self, stmts: &[Stmt]) {
        self.push();
        self.stmts(stmts);
        self.pop();
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { name, ty, value } => {
                let mut t = self.infer(value);
                if let Some(ty) = ty {
                    let d = self.decl_ty(ty, &["self".into()]);
                    self.assign(&d, &t, value.span, "variable");
                    t = d;
                }
                // keep the variable as the root of its own units: `let x = …` then `x.total`
                self.bind(name, t);
            }
            StmtKind::If {
                binding,
                cond,
                then,
                otherwise,
            } => {
                let ct = self.infer(cond);
                // `if x:` on a value that is not true or false would never run its block
                if binding.is_none()
                    && matches!(
                        ct.unopt(),
                        T::Text(_) | T::Num(_) | T::Ent(_) | T::Date | T::DateTime
                    )
                {
                    self.error(
                        cond.span,
                        "`if` takes a condition, true or false: for a value that may be missing, `if let v = x:` (or `if x != none:`)",
                    );
                }
                let facts = self.facts_true(cond);
                self.push();
                if let Some(b) = binding {
                    self.bind(b, ct.unopt().clone());
                }
                let n = self.narrowed.len();
                self.narrowed.extend(facts);
                self.stmts(then);
                self.narrowed.truncate(n);
                self.pop();
                if let Some(o) = otherwise {
                    let facts = self.facts_false(cond);
                    let n = self.narrowed.len();
                    self.narrowed.extend(facts);
                    self.block(o);
                    self.narrowed.truncate(n);
                }
            }
            StmtKind::For { names, iter, body } => {
                let it = self.infer(iter);
                let elem = match it.unopt() {
                    T::Coll { elem, .. } => (**elem).clone(),
                    T::Table(n) => T::Ent(n.clone()),
                    _ => T::Unknown,
                };
                self.push();
                if names.len() == 1 {
                    self.bind(&names[0], elem);
                } else {
                    for (i, n) in names.iter().enumerate() {
                        let t = match &elem {
                            T::Tuple(items) => {
                                items.get(i).map(|(_, t)| t.clone()).unwrap_or(T::Unknown)
                            }
                            _ => T::Unknown,
                        };
                        self.bind(n, t);
                    }
                }
                self.stmts(body);
                self.pop();
            }
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    let t = self.infer(e);
                    if let Some(ret) = self.fn_ret.clone() {
                        self.assign(&ret, &t, e.span, "return value");
                    }
                }
            }
            StmtKind::Require { cond, message } => {
                self.infer(cond);
                if let Some(m) = message {
                    self.in_fstring += 1;
                    self.infer(m);
                    self.in_fstring -= 1;
                }
            }
            StmtKind::Fail(e) | StmtKind::Expect(e) | StmtKind::Clock(e) | StmtKind::Expr(e) => {
                self.infer(e);
            }
            StmtKind::Emit(e) => {
                self.infer(e);
            }
            StmtKind::Receive { inbound, body, .. } => {
                if !self.m.inbounds.iter().any(|i| &i.name == inbound) {
                    self.error(s.span, format!("no inbound `{inbound}`: `receive` runs the code of an inbound on a message"));
                }
                let t = self.infer(body);
                if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
                    self.error(body.span, "`receive` takes the message as text (`receive OrderIn \"{…}\"`) or a file (`receive OrderIn file \"tests/order.json\"`)");
                }
            }
            StmtKind::Deliver {
                event,
                reply,
                failed,
            } => {
                let m = self.m;
                match m.outbounds.iter().find(|o| &o.event == event) {
                    None => self.error(
                        s.span,
                        format!("`{event}` has no `outbound`: nothing delivers it"),
                    ),
                    Some(o) => {
                        if let Some(r) = reply {
                            let t = self.infer(r);
                            match &o.reply {
                                None => self.error(r.span, format!("the outbound of `{event}` has no `reply`: nothing reads the answer")),
                                Some(want) => {
                                    if !matches!(t.unopt(), T::Ent(n) if n == want) && t != T::Unknown {
                                        self.error(r.span, format!("the reply of `{event}` is a {want}"));
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(f) = failed {
                    let t = self.infer(f);
                    if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
                        self.error(f.span, "`deliver … failed` takes the error, as text");
                    }
                }
            }
            StmtKind::ExpectSent { event, .. } => {
                let known = match &event.kind {
                    ExprKind::Call { callee, .. } | ExprKind::Field { base: callee, .. } => {
                        matches!(&callee.kind, ExprKind::Name(n) if self.m.events.iter().any(|e| &e.name == n))
                    }
                    ExprKind::Name(n) => self.m.events.iter().any(|e| &e.name == n),
                    _ => false,
                };
                if !known {
                    self.error(event.span, "`expect sent` takes an event, with the fields to match: `expect sent OrderPlaced(order=o)`");
                } else if matches!(event.kind, ExprKind::Call { .. }) {
                    // the fields given, typed as the event's; the others are not needed
                    if let ExprKind::Call { callee, args } = &event.kind {
                        let ExprKind::Name(n) = &callee.kind else {
                            unreachable!()
                        };
                        let ev = self.m.events.iter().find(|e| &e.name == n).cloned();
                        for a in args {
                            let t = self.infer(&a.value);
                            match (a.name.as_deref(), &ev) {
                                (Some(f), Some(ev)) => match ev.fields.iter().find(|(x, _)| x == f)
                                {
                                    None => self
                                        .error(a.value.span, format!("`{n}` has no field `{f}`")),
                                    Some((_, ty)) => {
                                        let want = self.decl_ty(ty, &["self".into()]);
                                        self.assign(&want, &t, a.value.span, "field");
                                    }
                                },
                                _ => self.error(
                                    a.value.span,
                                    "the fields to match are named: `OrderPlaced(order=o)`",
                                ),
                            }
                        }
                    }
                }
            }
            StmtKind::ExpectError { body, .. } => self.block(body),
            StmtKind::ExpectCompileError { .. } => {}
            StmtKind::Use(name) => {
                if let Some(scope) = self.fixture_scopes.get(name).cloned() {
                    if let Some(s) = self.scopes.last_mut() {
                        s.extend(scope);
                    }
                }
            }
            StmtKind::As { body, .. } => self.stmts(body),
            StmtKind::Assign { target, value, .. } => {
                let vt = self.infer(value);
                let tt = self.infer(target);
                if let ExprKind::Field { base, name, .. } = &target.kind {
                    let bt = self.type_only(base);
                    if let T::Ent(en) = bt.unopt() {
                        let en2 = en.clone();
                        self.record_write(&en2, name, target.span);
                        if let Some(f) = self.m.entity(en).and_then(|e| self.m.find_field(e, name))
                        {
                            if f.kind == FieldKind::Formula {
                                self.error(
                                    target.span,
                                    format!("`{name}` is a formula: it cannot be assigned"),
                                );
                            }
                        }
                        // the field of a flow moves by its transitions (migrations and changes,
                        // where data fixes may set it, are not checked statement by statement)
                        if let Some(msg) = self
                            .m
                            .entity(en)
                            .and_then(|e| self.m.flow_assignment_error(e, name))
                        {
                            self.error(target.span, msg);
                        }
                    }
                } else if let ExprKind::Name(name) = &target.kind {
                    if self.local(name).is_none() {
                        if let Some(en) = self.entity {
                            let en = en.name.clone();
                            self.record_write(&en, name, target.span);
                        }
                        if let Some(f) = self.entity.and_then(|e| self.m.find_field(e, name)) {
                            if f.kind == FieldKind::Formula {
                                self.error(
                                    target.span,
                                    format!("`{name}` is a formula: it cannot be assigned"),
                                );
                            }
                        }
                        if let Some(msg) = self
                            .entity
                            .and_then(|e| self.m.flow_assignment_error(e, name))
                        {
                            self.error(target.span, msg);
                        }
                    }
                }
                self.assign(&tt, &vt, value.span, "field");
            }
            StmtKind::Meta { value, .. } => {
                if !matches!(&value.kind, ExprKind::Name(_)) {
                    self.infer(value);
                }
            }
            StmtKind::Block { body, .. } => self.block(body),
            StmtKind::Fn(f) => {
                if let Some(s) = self.scopes.last_mut() {
                    s.insert(
                        f.name.clone(),
                        Local {
                            ty: T::Unknown,
                            func: Some(f.clone()),
                        },
                    );
                }
                let saved = self.fn_ret.take();
                self.check_fn(f);
                self.fn_ret = saved;
            }
            StmtKind::Migration { .. } => {}
        }
    }

    // ---- narrowing --------------------------------------------------------------------

    /// Paths known to be non-none when `cond` is true.
    fn facts_true(&self, cond: &Expr) -> Vec<String> {
        match &cond.kind {
            ExprKind::Binary {
                op: BinOp::And,
                lhs,
                rhs,
            } => {
                let mut v = self.facts_true(lhs);
                v.extend(self.facts_true(rhs));
                v
            }
            ExprKind::Binary {
                op: BinOp::Ne,
                lhs,
                rhs,
            } if is_none(rhs) => self
                .path(lhs)
                .map(|p| vec![path_str(&p)])
                .unwrap_or_default(),
            ExprKind::Binary {
                op: BinOp::Is | BinOp::Eq,
                lhs,
                rhs,
            } if !is_none(rhs) => {
                // `x?.kind is Kit` → x is not none
                let mut out = Vec::new();
                let mut cur = lhs.as_ref();
                while let ExprKind::Field { base, optional, .. } = &cur.kind {
                    if *optional {
                        if let Some(p) = self.path(base) {
                            out.push(path_str(&p));
                        }
                    }
                    cur = base;
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Paths known to be non-none when `cond` is false.
    fn facts_false(&self, cond: &Expr) -> Vec<String> {
        match &cond.kind {
            ExprKind::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
            } => {
                let mut v = self.facts_false(lhs);
                v.extend(self.facts_false(rhs));
                v
            }
            ExprKind::Binary {
                op: BinOp::Eq,
                lhs,
                rhs,
            } if is_none(rhs) => self
                .path(lhs)
                .map(|p| vec![path_str(&p)])
                .unwrap_or_default(),
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => self.facts_true(expr),
            _ => Vec::new(),
        }
    }

    // ---- paths ------------------------------------------------------------------------

    /// Path of an expression from a root, with collection facts applied.
    fn path(&self, e: &Expr) -> Option<Vec<String>> {
        let raw = match &e.kind {
            ExprKind::SelfRef => Some(vec!["self".to_string()]),
            ExprKind::Name(n) => {
                if self.local(n).is_some() || n == "current" || n == "company" && self.company_ctx {
                    Some(vec![n.clone()])
                } else if self
                    .entity
                    .map_or(false, |en| self.m.find_field(en, n).is_some())
                {
                    Some(vec!["self".to_string(), n.clone()])
                } else {
                    None
                }
            }
            ExprKind::Field { base, name, .. } => {
                let mut p = self.path(base)?;
                p.push(name.clone());
                Some(p)
            }
            _ => None,
        }?;
        Some(self.normalize(raw))
    }

    fn normalize(&self, p: Vec<String>) -> Vec<String> {
        let q = self.apply_facts(p);
        // equalities stated by `where` on reference fields of the root's entity:
        // `invoice: Invoice where i => i.currency == payment.currency`
        let Some(root) = q.first().cloned() else {
            return q;
        };
        let entity = match self.local(&root).map(|l| &l.ty) {
            Some(T::Ent(e)) => self.m.entity(e),
            _ if root == "self" => self.entity,
            _ => None,
        };
        if let Some(e) = entity {
            for (a, b) in self.entity_equalities(e) {
                for (from, to) in [(&b, &a), (&a, &b)] {
                    if q.len() > from.len() && q[1..=from.len()] == from[..] {
                        let mut cand = vec![root.clone()];
                        cand.extend(to.iter().cloned());
                        cand.extend(q[from.len() + 1..].iter().cloned());
                        let cand = self.apply_facts(cand);
                        if cand.first() != Some(&root) {
                            return cand;
                        }
                    }
                }
            }
        }
        q
    }

    fn apply_facts(&self, p: Vec<String>) -> Vec<String> {
        let Some(root) = p.first() else { return p };
        if let Some(facts) = self.facts.get(root) {
            for f in facts {
                if p.len() > f.rel.len() && p[1..=f.rel.len()] == f.rel[..] {
                    let mut out = f.target.clone();
                    out.extend(p[f.rel.len() + 1..].iter().cloned());
                    return self.apply_facts(out);
                }
            }
        }
        p
    }

    /// `[field, x…] == [y…]` for each `field: E where v => v.x… == y…` of the entity.
    fn entity_equalities(&self, e: &Entity) -> Vec<(Vec<String>, Vec<String>)> {
        let mut out = Vec::new();
        for f in self.m.all_fields(e) {
            let Some(Where::Expr(Expr {
                kind: ExprKind::Lambda { params, body },
                ..
            })) = &f.where_
            else {
                continue;
            };
            let Some(v) = params.first() else { continue };
            let mut stack = vec![body.as_ref()];
            while let Some(x) = stack.pop() {
                match &x.kind {
                    ExprKind::Binary {
                        op: BinOp::And,
                        lhs,
                        rhs,
                    } => {
                        stack.push(lhs);
                        stack.push(rhs);
                    }
                    ExprKind::Binary {
                        op: BinOp::Eq,
                        lhs,
                        rhs,
                    } => {
                        for (a, b) in [(lhs, rhs), (rhs, lhs)] {
                            if let (Some(rel), None) = (rel_path(a, v), rel_path(b, v)) {
                                if let Some(other) = entity_rel_path(b, e, self.m) {
                                    let mut left = vec![f.name.clone()];
                                    left.extend(rel);
                                    out.push((left, other));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    // ---- expressions ------------------------------------------------------------------

    /// Infer without reporting (used to look at a receiver twice).
    fn type_only(&mut self, e: &Expr) -> T {
        let n = self.diags.len();
        let t = self.infer(e);
        self.diags.truncate(n);
        t
    }

    fn infer(&mut self, e: &Expr) -> T {
        match &e.kind {
            ExprKind::Lit(l) => match l {
                Lit::Number(n) if n == "0" => T::Zero,
                Lit::Number(n) if n.contains('.') => T::Num(Num::Decimal),
                Lit::Number(_) => T::Num(Num::Int),
                Lit::Percent(_) => T::Num(Num::Percent),
                Lit::Str(_) => T::text(),
                Lit::Date(_) => T::Date,
                Lit::DateTime(_) => T::DateTime,
                Lit::YearMonth(_) => T::YearMonth,
                Lit::Bool(_) => T::Bool,
                Lit::None => T::None,
            },
            ExprKind::FString(parts) => {
                self.in_fstring += 1;
                for p in parts {
                    if let FPart::Expr(x, spec) = p {
                        let t = self.infer(x);
                        if let Some(spec) = spec {
                            self.check_format(&t, spec, x.span);
                        }
                    }
                }
                self.in_fstring -= 1;
                T::text()
            }
            ExprKind::Quantity { unit, per, .. } => self.quantity(unit, per.as_deref(), e.span),
            ExprKind::Name(n) => self.name(n, e.span),
            ExprKind::SelfRef => self
                .entity
                .map(|en| T::Ent(en.name.clone()))
                .unwrap_or(T::Unknown),
            ExprKind::Field {
                base,
                name,
                optional,
            } => self.field_access(base, name, *optional, e.span),
            ExprKind::Call { callee, args } => self.call(callee, args, e.span),
            ExprKind::Index { base, index } => {
                // per-company member at a given company: `product.cost[company]`
                if let ExprKind::Field {
                    base: recv, name, ..
                } = &base.kind
                {
                    let rt = self.type_only(recv);
                    if let T::Ent(en) = rt.unopt() {
                        if let Some(f) = self
                            .m
                            .entity(en)
                            .and_then(|ent| self.m.find_field(ent, name))
                        {
                            if f.per_company {
                                self.infer(index);
                                let rp = self.path(recv);
                                let cp = self.path(index);
                                let en = en.clone();
                                return self.member_type(&en, name, rp, cp, e.span);
                            }
                        }
                    }
                }
                let bt = self.infer(base);
                self.infer(index);
                match bt.unopt() {
                    T::Coll { elem, .. } => (**elem).clone(),
                    _ => T::Unknown,
                }
            }
            ExprKind::Unary { op, expr } => {
                let t = self.infer(expr);
                match op {
                    UnOp::Not => T::Bool,
                    UnOp::Neg => t,
                }
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, e.span),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.infer(cond);
                let n = self.narrowed.len();
                let ft = self.facts_true(cond);
                self.narrowed.extend(ft);
                let a = self.infer(then);
                self.narrowed.truncate(n);
                let ff = self.facts_false(cond);
                self.narrowed.extend(ff);
                let b = self.infer(otherwise);
                self.narrowed.truncate(n);
                unify(a, b)
            }
            ExprKind::Lambda { params, body } => {
                self.push();
                for p in params {
                    self.bind(p, T::Unknown);
                }
                let r = self.infer(body);
                self.pop();
                T::Fn {
                    params: params.len(),
                    ret: Box::new(r),
                }
            }
            ExprKind::Tuple(items) => T::Tuple(
                items
                    .iter()
                    .map(|(n, x)| (n.clone(), self.infer(x)))
                    .collect(),
            ),
            ExprKind::List(items) => {
                let mut elem = T::Unknown;
                for a in items {
                    let t = self.infer(&a.value);
                    let t = if a.spread {
                        match t {
                            T::Coll { elem, .. } => *elem,
                            other => other,
                        }
                    } else {
                        t
                    };
                    elem = if elem.is_unknown() { t } else { elem };
                }
                T::list(elem)
            }
            ExprKind::Set(items) => {
                let mut elem = T::Unknown;
                for x in items {
                    let t = self.infer(x);
                    elem = match (&elem, t) {
                        (T::Unknown, t) => t,
                        (_, T::Coll { elem: inner, .. }) if elem.is_unknown() => *inner,
                        _ => elem,
                    };
                }
                T::Coll {
                    elem: Box::new(elem),
                    set: true,
                    facts: Vec::new(),
                }
            }
            ExprKind::When(arms) => {
                let mut out = T::Unknown;
                for (c, v) in arms {
                    if let Some(c) = c {
                        self.infer(c);
                    }
                    let t = self.infer(v);
                    out = unify(out, t);
                }
                out
            }
            ExprKind::Match { subject, arms } => self.match_expr(subject, arms, e.span),
            ExprKind::Block(stmts) => {
                self.push();
                let mut last = T::Unknown;
                for (i, s) in stmts.iter().enumerate() {
                    if i + 1 == stmts.len() {
                        if let StmtKind::Expr(x) = &s.kind {
                            last = self.infer(x);
                            continue;
                        }
                    }
                    self.stmt(s);
                }
                self.pop();
                last
            }
            ExprKind::As { body, .. } => self.infer(body),
            ExprKind::Fail(x) => {
                self.in_fstring += 1;
                self.infer(x);
                self.in_fstring -= 1;
                T::Unknown
            }
            ExprKind::Error => T::Unknown,
        }
    }

    fn quantity(&mut self, unit: &str, per: Option<&str>, span: Span) -> T {
        if Self::is_duration_unit(unit) {
            return T::Duration;
        }
        let known = |c: &Self, u: &str| c.units.contains_key(u) || c.currencies.contains(u);
        if !known(self, unit) {
            self.error(span, format!("unknown unit or currency `{unit}`"));
            return T::Unknown;
        }
        if let Some(p) = per {
            if !self.units.contains_key(p) {
                self.error(span, format!("unknown unit `{p}`"));
            }
            return T::Price {
                cur: Unit::Const(unit.into()),
                per: Unit::Const(p.into()),
                exact: false,
            };
        }
        if self.currencies.contains(unit) {
            T::Money {
                cur: Unit::Const(unit.into()),
                exact: false,
            }
        } else {
            T::Qty {
                unit: Unit::Const(unit.into()),
                exact: false,
            }
        }
    }

    fn name(&mut self, n: &str, span: Span) -> T {
        if let Some(l) = self.local(n) {
            return l.ty.clone();
        }
        if n == "env" && self.is_env(n) {
            self.error(
                span,
                "`env` is read one variable at a time: `env.NAME`, a variable of quadra.toml ([vars])",
            );
            return T::Unknown;
        }
        match n {
            "me" => return T::Ent("User".into()),
            "current" => return T::Current,
            "company" if self.company_ctx => return T::Ent("Company".into()),
            "owner" => return T::Unknown,
            _ => {}
        }
        if let Some(en) = self.entity {
            if self.m.find_field(en, n).is_some() {
                let ename = en.name.clone();
                let company = if self.company_ctx {
                    Some(vec!["company".to_string()])
                } else {
                    None
                };
                return self.member_type(&ename, n, Some(vec!["self".into()]), company, span);
            }
            if en.actions.iter().any(|a| a.name == n) || en.methods.iter().any(|f| f.name == n) {
                return T::Unknown;
            }
            match n {
                "id" => return T::text(),
                "created_at" | "updated_at" => return T::DateTime,
                "created_by" | "updated_by" => return T::Ent("User".into()),
                "version" => return T::Num(Num::Int),
                "history" => {
                    self.error(span, "`history` is not available in expressions: read it through the API (`GET /v1/records/{entity}/{id}/history`)");
                    return T::Unknown;
                }
                _ => {}
            }
        }
        if self.m.entity(n).is_some() {
            return T::Table(n.to_string());
        }
        if self.m.variant(n).is_some() {
            return T::Variant(n.to_string());
        }
        if let Some(c) = self.m.constant(n) {
            return c
                .ty
                .as_ref()
                .map(|t| self.decl_ty(t, &["self".into()]))
                .unwrap_or(T::Unknown);
        }
        if self.m.function(n).is_some() || self.m.event(n).is_some() || BUILTIN_FNS.contains(&n) {
            return T::Unknown;
        }
        if self.currencies.contains(n) {
            return T::Ent("Currency".into());
        }
        if self.units.contains_key(n) {
            return T::Ent("Uom".into());
        }
        if self.m.role(n).is_some() {
            return T::Role;
        }
        if [
            "Text",
            "Code",
            "Int",
            "Decimal",
            "Money",
            "Qty",
            "Price",
            "DateRange",
        ]
        .contains(&n)
        {
            return T::Unknown;
        }
        self.error(span, format!("unknown name `{n}`"));
        T::Unknown
    }

    /// Type of `receiver.field` for an entity field, rebased on the receiver's path.
    fn member_type(
        &mut self,
        entity: &str,
        field: &str,
        receiver: Option<Vec<String>>,
        company: Option<Vec<String>>,
        span: Span,
    ) -> T {
        self.record_read(entity, field, span);
        if self.m.entity(entity).map_or(false, |e| e.is_ledger) {
            self.record_read(entity, "*", span);
        }
        let Some(e) = self.m.entity(entity) else {
            return T::Unknown;
        };
        let Some(f) = self.m.find_field(e, field) else {
            return T::Unknown;
        };
        let local = if let Some(ty) = &f.ty {
            let saved_entity = self.entity;
            let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
            self.entity = Some(e);
            let t = self.decl_ty_field(ty, f.per_company);
            self.entity = saved_entity;
            self.scopes = saved_scopes;
            t
        } else if f.formula.is_some() {
            self.formula_type(e, f)
        } else {
            T::Unknown
        };
        let mut t = self.rebase(local, receiver.as_deref(), company.as_deref());
        // a `T? required when …` field is read as T: the interpreter enforces it
        if f.required_when.is_some() {
            t = t.unopt().clone();
        }
        // collections of children carry the fact `child.<back-ref> == receiver`
        if let T::Coll { elem, set, .. } = &t {
            if let (T::Ent(child), Some(rp)) = (elem.as_ref(), &receiver) {
                if let Some(back) = self.back_ref(e, f, child) {
                    let facts = vec![Fact {
                        rel: vec![back],
                        target: rp.clone(),
                    }];
                    t = T::Coll {
                        elem: elem.clone(),
                        set: *set,
                        facts,
                    };
                }
            }
        }
        let _ = span;
        t
    }

    /// The field of `child` that points back to the owner of the collection field `f`.
    fn back_ref(&self, owner: &Entity, f: &Field, child: &str) -> Option<String> {
        if let Some(v) = &f.via {
            return Some(v.clone());
        }
        let ce = self.m.entity(child)?;
        ce.fields
            .iter()
            .find(|cf| {
                match cf.ty.as_ref().map(|t| match t {
                    Ty::Optional(i) => i.as_ref(),
                    other => other,
                }) {
                    Some(Ty::Path { segments, .. }) => {
                        segments.len() == 1
                            && (segments[0] == owner.name || owner.traits.contains(&segments[0]))
                    }
                    _ => false,
                }
            })
            .map(|cf| cf.name.clone())
    }

    /// Inferred type of an untyped formula, relative to its own entity (`self`).
    fn formula_type(&mut self, e: &'m Entity, f: &'m Field) -> T {
        let key = (e.name.clone(), f.name.clone());
        if let Some(t) = self.formula_cache.get(&key) {
            return t.clone();
        }
        if !self.in_progress.insert(key.clone()) {
            return T::Unknown;
        }
        let saved = (
            self.entity,
            std::mem::replace(&mut self.scopes, vec![HashMap::new()]),
            std::mem::take(&mut self.facts),
            std::mem::take(&mut self.narrowed),
            self.company_ctx,
            self.in_formula,
        );
        self.entity = Some(e);
        self.company_ctx = f.per_company;
        self.in_formula = true;
        let saved_current = self.current.replace((e.name.clone(), f.name.clone()));
        let n = self.diags.len();
        let t = self.infer(f.formula.as_ref().unwrap());
        self.current = saved_current;
        self.diags.truncate(n); // reported when the entity itself is checked
        self.entity = saved.0;
        self.scopes = saved.1;
        self.facts = saved.2;
        self.narrowed = saved.3;
        self.company_ctx = saved.4;
        self.in_formula = saved.5;
        self.in_progress.remove(&key);
        self.formula_cache.insert(key, t.clone());
        t
    }

    /// Replace `self` in unit paths with the receiver's path, `company` with the company.
    fn rebase(&self, t: T, receiver: Option<&[String]>, company: Option<&[String]>) -> T {
        let re = |u: Unit| -> Unit {
            match u {
                Unit::Path(p) => {
                    let root = p[0].as_str();
                    let (prefix, rest): (Option<Vec<String>>, &[String]) = match root {
                        "self" => (receiver.map(|r| r.to_vec()), &p[1..]),
                        "company" => (
                            Some(
                                company
                                    .map(|c| c.to_vec())
                                    .unwrap_or_else(|| vec!["current".into(), "company".into()]),
                            ),
                            &p[1..],
                        ),
                        "owner" => (
                            receiver
                                .filter(|r| r.len() > 1)
                                .map(|r| r[..r.len() - 1].to_vec()),
                            &p[1..],
                        ),
                        "current" => (Some(vec![]), &p[..]),
                        _ => (Some(vec![]), &p[..]),
                    };
                    match prefix {
                        Some(mut pre) => {
                            pre.extend(rest.iter().cloned());
                            Unit::Path(self.normalize(pre))
                        }
                        None => Unit::Unknown,
                    }
                }
                other => other,
            }
        };
        map_units(t, &re)
    }

    /// Whether the name `env` is the environment here (not a local or a field called `env`).
    fn is_env(&self, n: &str) -> bool {
        self.local(n).is_none()
            && self
                .entity
                .is_none_or(|en| self.m.find_field(en, n).is_none())
    }

    /// `env.NAME`: a variable of quadra.toml, read where its value may differ from one place
    /// the app runs to another (actions, reactions, scripts), never where a value must be the
    /// same every time (formulas, checks, functions, constants), and never if it is a secret.
    fn env_var(&mut self, name: &str, span: Span) -> T {
        let Some(v) = self.m.manifest.var(name) else {
            self.error(
                span,
                format!("`env.{name}`: quadra.toml declares no variable {name} ([vars])"),
            );
            return T::Unknown;
        };
        if v.secret {
            self.error(span, format!("{name} is a secret: the code cannot read it (a connection of quadra.toml uses it for its credentials)"));
            return T::Unknown;
        }
        if self.in_formula || self.context.starts_with("fn ") || self.context.starts_with("const ")
        {
            self.error(span, format!("`env.{name}` is not allowed here: its value depends on where the app runs, and formulas, checks, functions and constants must give the same result everywhere (read it in an action or a reaction)"));
        }
        match v.ty {
            quadra_sema::manifest::VarType::Text => T::text(),
            quadra_sema::manifest::VarType::Integer => T::Num(Num::Int),
            quadra_sema::manifest::VarType::Decimal => T::Num(Num::Decimal),
            quadra_sema::manifest::VarType::Boolean => T::Bool,
        }
    }

    /// The value an outcome reaction gets (`on OrderShipped delivered as d:`): `d.event` (its
    /// fields), `d.at`, `d.attempts`, `d.status` (the HTTP status or SMTP reply), `d.response`
    /// (the beginning of the answer) and, when it failed, `d.error`. The event must have an
    /// `outbound`, else nothing would ever deliver it.
    fn outcome_type(&mut self, r: &quadra_ir::Reaction) -> T {
        let m = self.m;
        let Some(ev) = m.events.iter().find(|e| e.name == r.entity) else {
            self.error(
                r.span,
                format!("no event `{}`: `on … delivered` and `on … failed` are about the delivery of an event", r.entity),
            );
            return T::Unknown;
        };
        if !m.outbounds.iter().any(|o| o.event == ev.name) {
            self.error(
                r.span,
                format!("`{}` has no `outbound`: nothing delivers it, so it is never delivered or failed (add `outbound {} via <connection>`)", ev.name, ev.name),
            );
        }
        let fields: Vec<(Option<String>, T)> = ev
            .fields
            .iter()
            .map(|(n, ty)| (Some(n.clone()), self.decl_ty(ty, &["self".into()])))
            .collect();
        let mut out = vec![
            (Some("event".to_string()), T::Tuple(fields)),
            (Some("at".to_string()), T::DateTime),
            (Some("attempts".to_string()), T::Num(Num::Int)),
            (
                Some("status".to_string()),
                T::Opt(Box::new(T::Num(Num::Int))),
            ),
            (Some("response".to_string()), T::Opt(Box::new(T::text()))),
        ];
        if r.trigger == Trigger::Failed {
            out.push((Some("error".to_string()), T::text()));
        }
        // `reply T` on the outbound: the answer, read (none when it does not fit)
        if let Some(t) = m
            .outbounds
            .iter()
            .find(|o| o.event == ev.name)
            .and_then(|o| o.reply.clone())
        {
            out.push((Some("reply".to_string()), T::Opt(Box::new(T::Ent(t)))));
        }
        T::Tuple(out)
    }

    /// `record X from json:` (and `from xml`, `from csv(…)`, `from fixed`): a record, a known
    /// format with known options, fields of types the format can hold, each `at` in the syntax
    /// of the format; `at`, `format`, `decimals` and `ns` only there.
    fn check_record_format(&mut self, e: &'m Entity) {
        use quadra_ir::formats::{self, Column, Kind, Step};
        let m = self.m;
        let Some(fmt) = e.format.as_deref() else {
            for f in &e.fields {
                let what = if f.at.is_some() {
                    "`at` says where a field is in a document or a file"
                } else if f.pattern.is_some() {
                    "`format` says how a value is written in a document or a file"
                } else if f.decimals.is_some() {
                    "`decimals` says how a number is written in a document or a file"
                } else {
                    continue;
                };
                self.error(f.span, format!("{what}: only in a record `from` a format (`record {} from json:`, `from xml`, `from csv`, `from fixed`)", e.name));
            }
            for n in &e.namespaces {
                self.error(n.span, format!("`ns` declares a prefix of the paths of a record `from xml` (`record {} from xml:`)", e.name));
            }
            return;
        };
        if e.kind != quadra_ir::EntityKind::Record {
            self.error(e.span, format!("only a `record` is read from a format: `record {} from {fmt}:` (an entity has an identity and a history)", e.name));
            return;
        }
        if !formats::FORMATS.contains(&fmt) {
            self.error(
                e.span,
                format!("`{fmt}` is not a format Quadra reads (json, xml, csv, fixed)"),
            );
            return;
        }
        let opts = match formats::options(e) {
            Ok(o) => o,
            Err((span, why)) => {
                self.error(span, why);
                formats::Options::default()
            }
        };
        // the prefixes of a record `from xml`: its own `ns` lines (`xml:` is always bound)
        let mut prefixes: HashSet<Option<&str>> = HashSet::new();
        for n in &e.namespaces {
            if fmt != "xml" {
                self.error(n.span, format!("`ns` declares a prefix of the paths of a record `from xml`, not `from {fmt}`"));
            } else if n.uri.trim().is_empty() {
                self.error(n.span, "`ns` binds a prefix to the name of a namespace: `ns cbc \"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\"`");
            } else if matches!(n.prefix.as_deref(), Some("xml" | "xmlns")) {
                self.error(n.span, "`xml` and `xmlns` are prefixes of XML itself");
            } else if !prefixes.insert(n.prefix.as_deref()) {
                let which = n
                    .prefix
                    .as_deref()
                    .map_or("the default namespace".to_string(), |p| format!("`{p}`"));
                self.error(n.span, format!("{which} is declared twice"));
            }
        }
        let declared = |q: &str| match formats::prefix(q) {
            None | Some("xml") => true,
            Some(p) => prefixes.contains(&Some(p)),
        };
        if let Some(root) = &opts.root {
            if !declared(root) {
                let span = e
                    .format_options
                    .iter()
                    .find(|o| o.name == "root")
                    .map_or(e.span, |o| o.span);
                self.error(span, format!("`root=\"{root}\"`: the prefix `{}` is not declared (`ns {} \"…\"` in the record)", formats::prefix(root).unwrap_or_default(), formats::prefix(root).unwrap_or_default()));
            }
        }
        let fields = formats::fields(m, e);
        let columns = formats::columns(&fields, opts.header);
        let mut seen_columns: HashMap<Column, &str> = HashMap::new();
        let mut ranges: Vec<((usize, usize), &str)> = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            let Some(ty) = &f.ty else { continue };
            let here = format!("`{}.{}`", e.name, f.name);
            let k = match formats::kind(m, ty, fmt) {
                Ok(k) => Some(k),
                Err(why) => {
                    self.error(f.span, format!("{here}: {why}"));
                    None
                }
            };
            // what a single value is: the items of a list
            let item = k.as_ref().map(|k| match k.unopt() {
                Kind::List(i) => i.unopt().clone(),
                k => k.clone(),
            });
            match fmt {
                "json" => {
                    if let Some(p) = &f.at {
                        if p.is_empty() || p.split('.').any(|k| k.is_empty()) {
                            self.error(f.span, format!("`at \"{p}\"`: keys separated by dots (`shipping.zip`)"));
                        }
                    }
                }
                "xml" => match formats::xml_path(f) {
                    Err(why) => self.error(f.span, why),
                    Ok(steps) => {
                        for s in &steps {
                            if let Step::Element(q) | Step::Attribute(q) = s {
                                if !declared(q) {
                                    let p = formats::prefix(q).unwrap_or_default();
                                    self.error(f.span, format!("`at \"{}\"`: the prefix `{p}` is not declared (`ns {p} \"…\"` in `{}`: each record declares the prefixes of its paths)", f.at.as_deref().unwrap_or_default(), e.name));
                                }
                            }
                        }
                        let attribute = steps.iter().any(|s| matches!(s, Step::Attribute(_)));
                        let own_text = !steps.iter().any(|s| matches!(s, Step::Element(_)));
                        match &item {
                            Some(Kind::Record(_)) if attribute || own_text => self.error(f.span, format!("{here}: a record is an element, not an attribute or the text of this one")),
                            _ if matches!(k.as_ref().map(|k| k.unopt()), Some(Kind::List(_))) && !steps.iter().any(|s| matches!(s, Step::Element(_))) => self.error(f.span, format!("{here}: a list is repeated elements: `at \"Item\"` or `at \"Item/@id\"`")),
                            _ => {}
                        }
                    }
                },
                "csv" => match &columns[i] {
                    Err(why) => self.error(f.span, why.clone()),
                    Ok(c) => {
                        if let Some(other) = seen_columns.insert(c.clone(), &f.name) {
                            self.error(f.span, format!("{here}: column {c} is already `{other}`"));
                        }
                    }
                },
                _ => match f.at.as_deref() {
                    None => self.error(f.span, format!("{here}: a field of a fixed-width line says its columns: `at \"11..20\"` (from 1, both included)")),
                    Some(at) => match formats::range(at) {
                        Err(why) => self.error(f.span, why),
                        Ok(r) => {
                            if let Some((o, other)) = ranges.iter().find(|(o, _)| o.0 <= r.1 && r.0 <= o.1) {
                                self.error(f.span, format!("{here}: columns {}..{} overlap those of `{other}` ({}..{})", r.0, r.1, o.0, o.1));
                            }
                            ranges.push((r, &f.name));
                            if item == Some(Kind::Decimal) && f.decimals.is_none() {
                                self.error(f.span, format!("{here}: a number with decimals in a fixed-width line says how many of its digits are decimals: `decimals 2` (`0001250` is 12.50)"));
                            }
                        }
                    },
                },
            }
            if let Some(p) = &f.pattern {
                match &item {
                    Some(Kind::Date | Kind::DateTime) => {
                        let bad = chrono::format::StrftimeItems::new(p)
                            .any(|i| matches!(i, chrono::format::Item::Error));
                        if bad || p.is_empty() {
                            self.error(f.span, format!("`format \"{p}\"`: a pattern of dates: `%d/%m/%Y`, `%Y%m%d`, `%d.%m.%Y %H:%M`"));
                        }
                    }
                    Some(Kind::Bool) => {
                        let ok = p.split_once('/').is_some_and(|(t, f)| {
                            !t.is_empty() && !f.is_empty() && t != f && !f.contains('/')
                        });
                        if !ok {
                            self.error(f.span, format!("`format \"{p}\"`: the text of true and of false: `format \"S/N\"`, `format \"Y/N\"`"));
                        }
                    }
                    Some(_) => self.error(
                        f.span,
                        format!(
                            "{here}: `format` says how a date, an instant or a boolean is written"
                        ),
                    ),
                    None => {}
                }
            }
            if let Some(d) = f.decimals {
                if item.is_some() && item != Some(Kind::Decimal) {
                    self.error(
                        f.span,
                        format!("{here}: `decimals` says how a `Decimal` is written"),
                    );
                } else if d > 28 {
                    self.error(f.span, format!("{here}: `decimals {d}`: at most 28"));
                }
            }
        }
    }

    /// The block of `outbound E via c as e:`: `e` is the event (its fields), the code reads but
    /// never writes, and its last expression is the request for the connection's kind
    /// (`post`, `put`, `patch`, `delete` over HTTP, `email` over SMTP). `reply` names a record
    /// `from json`.
    fn check_outbound_block(
        &mut self,
        o: &'m quadra_ir::Outbound,
        ev: &'m quadra_ir::Event,
        binding: &str,
        body: Option<&'m Expr>,
        kind: &str,
    ) {
        let fields: Vec<(Option<String>, T)> = ev
            .fields
            .iter()
            .map(|(n, ty)| (Some(n.clone()), self.decl_ty(ty, &["self".into()])))
            .collect();
        let saved = std::mem::replace(&mut self.context, format!("outbound {}", o.event));
        self.entity = None;
        self.push();
        // a batch: the binding is the list of its events
        if let Some((n, within)) = &o.batch {
            if !(1..=1000).contains(n) {
                self.error(o.span, "`batch N`: from 1 to 1000 events in one request");
            }
            if let Some(w) = within {
                let t = self.infer(w);
                if !matches!(t, T::Duration | T::Unknown)
                    || !matches!(w.kind, ExprKind::Quantity { .. })
                {
                    self.error(
                        w.span,
                        "`within` takes a fixed duration: `within 30 seconds`",
                    );
                }
            }
            if o.policy.ordered.is_some() || o.policy.key.is_some() {
                self.error(o.span, "a batch has no `ordered by` nor `key`: its events go together (`latest by` keeps only the newest of each)");
            }
            if o.reply.is_some() {
                self.error(
                    o.span,
                    "a batch has no `reply`: its answer is about all of its events",
                );
            }
            if body.is_none() {
                self.error(o.span, "a batch is sent by its block: it builds one request from the list of its events");
            }
        }
        // a `publish`: the record by its own name too
        if let Some(p) = &o.publishes {
            if let Some((_, t)) = fields.iter().find(|(k, _)| k.as_deref() == Some("record")) {
                self.bind(p, t.clone());
            }
        }
        // the lines of policy are about each event; the code of a batch, about the list
        self.bind(binding, T::Tuple(fields.clone()));
        self.check_policy(o);
        if o.batch.is_some() {
            self.bind(binding, T::list(T::Tuple(fields)));
        }
        if let Some(body) = body {
            self.infer(body);
        }
        self.pop();
        self.context = saved;
        let Some(body) = body else {
            return;
        };
        let ExprKind::Block(stmts) = &body.kind else {
            return;
        };
        // no write: the block only builds what is sent
        let mut w = Vec::new();
        writes(stmts, &mut w);
        for span in w {
            self.error(span, "the block of an `outbound` builds the request: it does not write (do it in the transaction that emits the event, or in `on … delivered`)");
        }
        // the last statement is the request, or an `if … else …` each branch of which ends so
        fn ends_with(stmts: &[quadra_ir::Stmt], ok: &dyn Fn(&str) -> bool) -> bool {
            match stmts.last().map(|s| &s.kind) {
                Some(StmtKind::Expr(e)) => match &e.kind {
                    ExprKind::Call { callee, .. } => {
                        matches!(&callee.kind, ExprKind::Name(n) if ok(n))
                    }
                    _ => false,
                },
                Some(StmtKind::If {
                    then,
                    otherwise: Some(o),
                    ..
                }) => ends_with(then, ok) && ends_with(o, ok),
                _ => false,
            }
        }
        let fits = ends_with(stmts, &|n| match kind {
            "http" => matches!(n, "post" | "put" | "patch" | "delete"),
            "smtp" => n == "email",
            _ => false,
        });
        if !fits {
            let what = if kind == "smtp" {
                "`email(to=…, subject=…, body=…)`"
            } else {
                "the request: `post(path, body)`, `put(…)`, `patch(…)` or `delete(path)`"
            };
            self.error(
                o.span,
                format!("the block of `outbound {}` ends with {what}", o.event),
            );
        }
        if let Some(r) = &o.reply {
            if kind != "http" {
                self.error(o.span, "`reply` reads the answer of an HTTP connection");
            } else if !self
                .m
                .entity(r)
                .is_some_and(|e| matches!(e.format.as_deref(), Some("json" | "xml")))
            {
                self.error(
                    o.span,
                    format!("`reply {r}`: a record `from json` or `from xml`, to read the answer"),
                );
            }
        }
    }

    /// `inbound Name via connection [path "…"] as m: Type:`: a record `from json`, an HTTP
    /// connection that says how its messages are signed, one handler per connection and path, a
    /// key of text; the handler is code as in an action.
    fn check_inbounds(&mut self) {
        let m = self.m;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for i in &m.inbounds {
            if !seen.insert((i.connection.clone(), i.path.clone().unwrap_or_default())) {
                self.error(
                    i.span,
                    format!(
                        "another inbound already takes the messages of `{}` at {}",
                        i.connection,
                        i.path.as_deref().unwrap_or("/")
                    ),
                );
            }
            match m.manifest.connection(&i.connection) {
                None => self.error(i.span, format!("no connection `{}` in quadra.toml ([connections.{}])", i.connection, i.connection)),
                Some(c) if c.kind_name() != "http" => self.error(i.span, format!("`{}` is not an HTTP connection: messages come in over HTTP", i.connection)),
                // a pull fetches its messages: only messages sent in prove their sender
                Some(c) if c.inbound.is_none() && (i.pull.is_none() || i.path.is_some()) => self.error(i.span, format!("`{}` does not say how its messages are signed (`inbound_signature` and `inbound_secret` in quadra.toml): without it anyone could send them", i.connection)),
                _ => {}
            }
            if let Some(p) = &i.path {
                if !p.starts_with('/') || p.contains("..") || p.chars().any(|c| c.is_whitespace()) {
                    self.error(
                        i.span,
                        format!("the path `{p}`: from `/`, no spaces, no `..`"),
                    );
                }
            }
            if !m.entity(&i.ty).is_some_and(|e| e.format.is_some()) {
                self.error(
                    i.span,
                    format!(
                        "`{}`: a message is read as a record `from json` (`record {} from json:`)",
                        i.ty, i.ty
                    ),
                );
            }
            if let Some(p) = &i.pull {
                let t = self.infer(&p.every);
                let fixed = matches!(&p.every.kind, ExprKind::Quantity { .. });
                if !matches!(t, T::Duration | T::Unknown) || !fixed {
                    self.error(
                        p.every.span,
                        "`every` takes a fixed duration: `every 5 minutes`",
                    );
                }
                if !p.path.starts_with('/')
                    || p.path.contains("..")
                    || p.path.chars().any(|c| c.is_whitespace())
                {
                    self.error(i.span, format!("the path `{}`: from `/`, no spaces, no `..` (the cursor as `{{cursor}}`)", p.path));
                }
                if p.path.contains("{cursor}") && p.cursor.is_none() {
                    self.error(
                        i.span,
                        "the path has `{cursor}`: say what it is with `cursor m.<field>`",
                    );
                }
                if let Some((page, field)) = &p.items {
                    let ok = m.entity(page).filter(|e| e.format.is_some()).and_then(|e| m.find_field(e, field)).is_some_and(|f| {
                        matches!(&f.ty, Some(Ty::List(t)) if matches!(t.as_ref(), Ty::Path { segments, .. } if segments.len() == 1 && segments[0] == i.ty))
                    });
                    if !ok {
                        self.error(i.span, format!("`items {page}.{field}`: a record `from json` whose `{field}` is a list of `{}`", i.ty));
                    }
                }
            }
            self.entity = None;
            self.context = format!("inbound {}", i.name);
            self.push();
            self.bind(&i.binding, T::Ent(i.ty.clone()));
            if let Some(c) = i.pull.as_ref().and_then(|p| p.cursor.as_ref()) {
                self.infer(c);
            }
            if let Some(k) = &i.key {
                let t = self.infer(k);
                if !matches!(t.unopt(), T::Text(_) | T::Num(_) | T::Unknown) {
                    self.error(
                        k.span,
                        "`key` is text (or a number): what tells two messages apart",
                    );
                }
            }
            self.stmts(&i.body);
            self.pop();
        }
    }

    /// `endpoint GET|POST "/path" via c as q: Query -> Answer:`: records `from json`, an HTTP
    /// connection that signs, one per method and path; the block ends with the answer; a `GET`
    /// writes nothing.
    fn check_endpoints(&mut self) {
        let m = self.m;
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        for ep in &m.endpoints {
            if !["GET", "POST"].contains(&ep.method.as_str()) {
                self.error(
                    ep.span,
                    format!(
                        "`{}`: an endpoint answers a GET (it reads) or a POST (it writes)",
                        ep.method
                    ),
                );
            }
            if !ep.path.starts_with('/')
                || ep.path.contains("..")
                || ep.path.chars().any(|c| c.is_whitespace())
            {
                self.error(
                    ep.span,
                    format!("the path `{}`: from `/`, no spaces, no `..`", ep.path),
                );
            }
            if !seen.insert((ep.connection.clone(), ep.method.clone(), ep.path.clone())) {
                self.error(
                    ep.span,
                    format!(
                        "another endpoint already answers {} {} of `{}`",
                        ep.method, ep.path, ep.connection
                    ),
                );
            }
            if ep.method == "POST"
                && m.inbounds.iter().any(|i| {
                    i.connection == ep.connection && i.path.as_deref().unwrap_or("") == ep.path
                })
            {
                self.error(
                    ep.span,
                    format!(
                        "an inbound already takes the messages of `{}` at {}",
                        ep.connection, ep.path
                    ),
                );
            }
            match m.manifest.connection(&ep.connection) {
                None => self.error(ep.span, format!("no connection `{}` in quadra.toml ([connections.{}])", ep.connection, ep.connection)),
                Some(c) if c.kind_name() != "http" => self.error(ep.span, format!("`{}` is not an HTTP connection", ep.connection)),
                Some(c) if c.inbound.is_none() => self.error(ep.span, format!("`{}` does not say how its requests are signed (`inbound_signature` and `inbound_secret` in quadra.toml): without it anyone could call it", ep.connection)),
                _ => {}
            }
            for t in [&ep.query, &ep.answer] {
                if !m.entity(t).is_some_and(|e| e.format.is_some()) {
                    self.error(
                        ep.span,
                        format!("`{t}`: a request and an answer are records `from json`"),
                    );
                }
            }
            self.entity = None;
            self.context = format!("endpoint {} {}", ep.method, ep.path);
            self.push();
            self.bind(&ep.binding, T::Ent(ep.query.clone()));
            if let Some(k) = &ep.key {
                self.infer(k);
            }
            let t = self.infer(&ep.body);
            self.pop();
            if !matches!(t.unopt(), T::Unknown) && t.unopt() != &T::Ent(ep.answer.clone()) {
                self.error(
                    ep.span,
                    format!(
                        "the block of the endpoint ends with its answer, a `{}`",
                        ep.answer
                    ),
                );
            }
            if ep.method == "GET" {
                if let ExprKind::Block(stmts) = &ep.body.kind {
                    let mut w = Vec::new();
                    writes(stmts, &mut w);
                    for span in w {
                        self.error(
                            span,
                            "a GET endpoint only reads: a write goes in a POST (or an `inbound`)",
                        );
                    }
                }
            }
        }
    }

    /// The lines of policy of an outbound: fixed durations, an integer of attempts, a window of
    /// two times of day, a condition, a key of text.
    fn check_policy(&mut self, o: &'m quadra_ir::Outbound) {
        let p = &o.policy;
        let fixed = |e: &Expr| -> bool {
            fn lit(e: &Expr) -> bool {
                match &e.kind {
                    ExprKind::Lit(_) | ExprKind::Quantity { .. } => true,
                    ExprKind::Binary { lhs, rhs, .. } => lit(lhs) && lit(rhs),
                    _ => false,
                }
            }
            lit(e)
        };
        for (what, e) in [
            ("retry for", &p.retry_for),
            ("expires after", &p.expires),
            ("delay", &p.delay),
        ] {
            if let Some(e) = e {
                let t = self.infer(e);
                if !matches!(t, T::Duration | T::Unknown) || !fixed(e) {
                    self.error(
                        e.span,
                        format!("`{what}` takes a fixed duration: `3 days`, `2 hours`"),
                    );
                }
            }
        }
        if let Some(e) = &p.retry_times {
            if !matches!(&e.kind, ExprKind::Lit(Lit::Number(n)) if n.parse::<u32>().is_ok_and(|n| n >= 1))
            {
                self.error(e.span, "`retry N times`: N is a whole number from 1");
            }
        }
        if let Some(e) = &p.backoff {
            let ok = matches!(&e.kind, ExprKind::Binary { op: BinOp::Range, lhs, rhs } if {
                let (a, b) = (self.infer(lhs), self.infer(rhs));
                matches!(a, T::Duration | T::Unknown) && matches!(b, T::Duration | T::Unknown) && fixed(lhs) && fixed(rhs)
            });
            if !ok {
                self.error(
                    e.span,
                    "`backoff` takes two fixed durations: `backoff 30 seconds .. 1 hour`",
                );
            }
        }
        if let Some(e) = &p.window {
            let time = |x: &Expr| matches!(&x.kind, ExprKind::Lit(Lit::Str(s)) if chrono::NaiveTime::parse_from_str(s, "%H:%M").is_ok());
            let ok = matches!(&e.kind, ExprKind::Binary { op: BinOp::Range, lhs, rhs } if time(lhs) && time(rhs));
            if !ok {
                self.error(e.span, "`window` takes two times of day: `window \"02:00\" .. \"04:00\"` (in the project's zone)");
            }
        }
        if let Some(e) = &p.unless {
            let t = self.infer(e);
            if !matches!(t.unopt(), T::Bool | T::Unknown) {
                self.error(
                    e.span,
                    "`unless` takes a condition: `unless e.order.state is Cancelled`",
                );
            }
        }
        if let Some(e) = &p.key {
            let t = self.infer(e);
            if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
                self.error(e.span, "`key` is text: `key f\"ship-{e.order.number}\"`");
            }
        }
        for e in [&p.ordered, &p.latest].into_iter().flatten() {
            self.infer(e);
        }
    }

    /// `outbound Event via connection`: the event and the connection exist, one outbound per
    /// event, and an email event has what an email needs.
    fn check_outbounds(&mut self) {
        let m = self.m;
        let mut seen: HashSet<&str> = HashSet::new();
        for o in &m.outbounds {
            let Some(ev) = m.events.iter().find(|e| e.name == o.event) else {
                self.error(
                    o.span,
                    format!(
                        "no event `{}` (declare it with `event {}(…)`)",
                        o.event, o.event
                    ),
                );
                continue;
            };
            if !seen.insert(o.event.as_str()) {
                self.error(
                    o.span,
                    format!(
                        "`{}` already has an outbound: an event goes out by one connection",
                        o.event
                    ),
                );
            }
            let Some(c) = m.manifest.connection(&o.connection) else {
                self.error(
                    o.span,
                    format!(
                        "no connection `{}` in quadra.toml ([connections.{}])",
                        o.connection, o.connection
                    ),
                );
                continue;
            };
            if let Some(binding) = &o.binding {
                self.check_outbound_block(o, ev, binding, o.body.as_ref(), c.kind_name());
            }
            match &c.kind {
                quadra_sema::manifest::ConnectionKind::Http { .. } => {
                    if let Some(p) = &o.path {
                        if !p.starts_with('/')
                            || p.contains("..")
                            || p.contains("://")
                            || p.chars().any(|c| c.is_whitespace())
                        {
                            self.error(o.span, format!("the path `{p}` is a path of the connection's url: from `/`, no spaces, no `..`"));
                        }
                    }
                }
                quadra_sema::manifest::ConnectionKind::Smtp { .. } => {
                    if o.path.is_some() {
                        self.error(
                            o.span,
                            "an email has no path: `outbound SendEmail via mail`",
                        );
                    }
                    // a block builds the email itself
                    if o.body.is_some() {
                        continue;
                    }
                    let text = |t: &Ty| matches!(t, Ty::Path { segments, .. } if segments.len() == 1 && ["Text", "Email"].contains(&segments[0].as_str()));
                    for need in ["to", "subject", "body"] {
                        match ev.fields.iter().find(|(n, _)| n == need) {
                            None => self.error(o.span, format!("an email event has `to`, `subject` and `body`: `{}` has no `{need}`", ev.name)),
                            Some((_, t)) if !text(t) => self.error(o.span, format!("`{}.{need}` is text (Text or Email) to go in an email", ev.name)),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn field_access(&mut self, base: &Expr, name: &str, optional: bool, span: Span) -> T {
        if let ExprKind::Name(b) = &base.kind {
            if b == "env" && self.is_env(b) {
                return self.env_var(name, span);
            }
        }
        let bt = self.infer(base);
        let bpath = self
            .path(base)
            .or_else(|| Some(vec![format!("#{}", u32::from(base.span.range.start()))]));
        let (inner, was_opt) = match &bt {
            T::Opt(i) => ((**i).clone(), true),
            other => (other.clone(), false),
        };
        if was_opt && !optional && self.in_fstring == 0 {
            let narrowed = bpath
                .as_ref()
                .map_or(false, |p| self.narrowed.contains(&path_str(p)));
            if !narrowed {
                self.error(
                    span,
                    format!(
                        "`{}` is optional: use `?.` (or check it is not none first)",
                        print::expr(base)
                    ),
                );
            }
        }
        self.record_access(&inner, name, span);
        let t = self.member(&inner, bpath, name, span);
        if optional && !matches!(t, T::Opt(_)) && was_opt {
            T::opt(t)
        } else {
            t
        }
    }

    fn member(&mut self, base: &T, bpath: Option<Vec<String>>, name: &str, span: Span) -> T {
        match base {
            T::Ent(en) => {
                if let Some(e) = self.m.entity(en) {
                    if self.m.find_field(e, name).is_some() {
                        let en = en.clone();
                        return self.member_type(&en, name, bpath, None, span);
                    }
                    if e.is_ledger && (name == "source" || name == "date") {
                        return if name == "date" {
                            T::DateTime
                        } else {
                            T::Unknown
                        };
                    }
                    match name {
                        "id" => return T::text(),
                        "created_at" | "updated_at" => return T::DateTime,
                        "created_by" | "updated_by" => return T::Ent("User".into()),
                        "version" => return T::Num(Num::Int),
                        "warnings" => return T::list(T::Unknown),
                        "history" => {
                            self.error(span, "`history` is not available in expressions: read it through the API (`GET /v1/records/{entity}/{id}/history`)");
                            return T::Unknown;
                        }
                        _ => {}
                    }
                    if e.actions.iter().any(|a| a.name == name)
                        || e.methods.iter().any(|f| f.name == name)
                        || self.is_flow_action(e, name)
                    {
                        return T::Unknown;
                    }
                    // implementors may add members to a trait value
                    if e.kind == EntityKind::Trait {
                        return T::Unknown;
                    }
                    let en = en.clone();
                    self.error(span, format!("`{en}` has no field `{name}`"));
                }
                T::Unknown
            }
            T::Money { .. } | T::Price { .. } | T::Qty { .. } | T::Num(_) | T::Zero
                if name == "raw" =>
            {
                T::Num(Num::Decimal)
            }
            T::Money { cur, .. } if name == "currency" => match cur {
                Unit::Path(p) => {
                    let _ = p;
                    T::Ent("Currency".into())
                }
                _ => T::Ent("Currency".into()),
            },
            T::Text(_) if name == "is_provisional" => T::Bool,
            T::Date | T::DateTime if name == "month" => T::YearMonth,
            T::Date | T::DateTime if name == "year" => T::Num(Num::Int),
            T::Date | T::DateTime if name == "weekday" => T::Num(Num::Int),
            T::DateTime if name == "hour" || name == "minute" => T::Num(Num::Int),
            T::DateTime if name == "date" => T::Date,
            T::Current => match name {
                "user" => T::Ent("User".into()),
                "company" => T::Ent("Company".into()),
                _ => {
                    self.error(
                        span,
                        format!("`current` has no `{name}` (use current.user or current.company)"),
                    );
                    T::Unknown
                }
            },
            T::Tuple(items) => {
                if let Some((_, t)) = items.iter().find(|(n, _)| n.as_deref() == Some(name)) {
                    return t.clone();
                }
                if items.iter().all(|(n, _)| n.is_some()) {
                    self.error(span, format!("this tuple has no field `{name}`"));
                }
                T::Unknown
            }
            T::Coll { .. } if name == "key" => self.group_key.clone().unwrap_or(T::Unknown),
            _ => T::Unknown,
        }
    }

    fn is_flow_action(&self, e: &Entity, name: &str) -> bool {
        e.flows
            .iter()
            .any(|f| f.transitions.iter().any(|t| t.by.as_deref() == Some(name)))
    }

    // ---- operators --------------------------------------------------------------------

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> T {
        let lt = self.infer(lhs);
        let n = self.narrowed.len();
        match op {
            BinOp::And | BinOp::Implies => {
                let f = self.facts_true(lhs);
                self.narrowed.extend(f);
            }
            BinOp::Or => {
                let f = self.facts_false(lhs);
                self.narrowed.extend(f);
            }
            _ => {}
        }
        let rt = self.infer(rhs);
        self.narrowed.truncate(n);
        match op {
            BinOp::Coalesce => unify(lt.unopt().clone(), rt),
            BinOp::And | BinOp::Or | BinOp::Implies | BinOp::In | BinOp::NotIn => T::Bool,
            BinOp::Is | BinOp::IsNot => {
                if let (T::Enum { name, variants }, T::Variant(v)) = (lt.unopt(), &rt) {
                    if !variants.contains(v) {
                        self.error(rhs.span, format!("`{v}` is not a value of {name}"));
                    }
                }
                T::Bool
            }
            BinOp::Range => T::list(lt),
            _ => self.binary_types(op, lt, rt, span),
        }
    }

    fn same_unit(&self, a: &Unit, b: &Unit, money: bool) -> bool {
        match (a, b) {
            (Unit::Const(x), Unit::Const(y)) => {
                if money {
                    x == y
                } else {
                    x == y || self.units.get(x).is_some() && self.units.get(x) == self.units.get(y)
                }
            }
            _ => true,
        }
    }

    fn binary_types(&mut self, op: BinOp, lt: T, rt: T, span: Span) -> T {
        use BinOp::*;
        let (l, r) = (lt.unopt().clone(), rt.unopt().clone());
        let verb = match op {
            Add => "add",
            Sub => "subtract",
            _ => "compare",
        };
        match op {
            Add | Sub | Eq | Ne | Lt | Le | Gt | Ge => {
                let cmp = !matches!(op, Add | Sub);
                let out = match (&l, &r) {
                    (T::Money { cur: a, exact: ea }, T::Money { cur: b, exact: eb }) => {
                        if !self.same_unit(a, b, true) {
                            self.error(span, format!("cannot {verb} {l} and {r}"));
                        }
                        T::Money {
                            cur: a.clone(),
                            exact: *ea || *eb,
                        }
                    }
                    (T::Qty { unit: a, exact: ea }, T::Qty { unit: b, exact: eb }) => {
                        if !self.same_unit(a, b, false) {
                            self.error(span, format!("cannot {verb} {l} and {r}"));
                        }
                        T::Qty {
                            unit: a.clone(),
                            exact: *ea || *eb,
                        }
                    }
                    (T::Price { cur: a, .. }, T::Price { cur: b, .. }) => {
                        if !self.same_unit(a, b, true) {
                            self.error(span, format!("cannot {verb} {l} and {r}"));
                        }
                        l.clone()
                    }
                    // durations: with durations, and with 0 (`hours > 0`); a bare number has no unit
                    (T::Duration, T::Duration)
                    | (T::Duration, T::Zero)
                    | (T::Zero, T::Duration) => T::Duration,
                    (T::Duration, T::Num(_)) | (T::Num(_), T::Duration) => {
                        self.error(span, format!("cannot {verb} {l} and {r}: write the unit, as in `5 hours`, `30 minutes`, `2 days`"));
                        T::Unknown
                    }
                    (T::Money { .. } | T::Qty { .. } | T::Price { .. }, T::Zero) => l.clone(),
                    (T::Zero, T::Money { .. } | T::Qty { .. } | T::Price { .. }) => r.clone(),
                    (T::Date, T::Duration) | (T::DateTime, T::Duration) => l.clone(),
                    (T::Date, T::Date) | (T::DateTime, T::DateTime) if op == Sub => T::Duration,
                    (T::Num(a), T::Num(b)) => T::Num(if *a == Num::Int && *b == Num::Int {
                        Num::Int
                    } else if *a == Num::Percent && *b == Num::Percent {
                        Num::Percent
                    } else {
                        Num::Decimal
                    }),
                    (T::Num(n), T::Zero) | (T::Zero, T::Num(n)) => T::Num(n.clone()),
                    (T::Zero, T::Zero) => T::Zero,
                    (T::Coll { .. }, _) => l.clone(),
                    (T::Text(_), T::Text(_)) => T::text(),
                    // different kinds of measure, or a measure and a plain number
                    (
                        T::Money { .. } | T::Qty { .. } | T::Price { .. },
                        T::Money { .. } | T::Qty { .. } | T::Price { .. } | T::Num(_),
                    )
                    | (T::Num(_), T::Money { .. } | T::Qty { .. } | T::Price { .. }) => {
                        self.error(span, format!("cannot {verb} {l} and {r}"));
                        T::Unknown
                    }
                    _ => T::Unknown,
                };
                if cmp {
                    T::Bool
                } else {
                    out
                }
            }
            Mul => match (&l, &r) {
                (T::Qty { unit, .. }, T::Price { cur, per, .. })
                | (T::Price { cur, per, .. }, T::Qty { unit, .. }) => {
                    if !self.same_unit(unit, per, false) {
                        let (a, b) = if matches!(l, T::Qty { .. }) {
                            (&l, &r)
                        } else {
                            (&r, &l)
                        };
                        self.error(span, format!("cannot multiply {a} by {b}"));
                    }
                    T::Money {
                        cur: cur.clone(),
                        exact: true,
                    }
                }
                // a product by an integer needs no rounding: it keeps the operand's exactness
                (T::Money { cur, exact }, T::Num(Num::Int))
                | (T::Num(Num::Int), T::Money { cur, exact }) => T::Money {
                    cur: cur.clone(),
                    exact: *exact,
                },
                (T::Money { cur, .. }, n) | (n, T::Money { cur, .. }) if n.is_numeric() => {
                    T::Money {
                        cur: cur.clone(),
                        exact: true,
                    }
                }
                (T::Price { cur, per, exact }, T::Num(Num::Int))
                | (T::Num(Num::Int), T::Price { cur, per, exact }) => T::Price {
                    cur: cur.clone(),
                    per: per.clone(),
                    exact: *exact,
                },
                (T::Price { cur, per, .. }, n) | (n, T::Price { cur, per, .. })
                    if n.is_numeric() =>
                {
                    T::Price {
                        cur: cur.clone(),
                        per: per.clone(),
                        exact: true,
                    }
                }
                (T::Qty { unit, exact }, T::Num(Num::Int))
                | (T::Num(Num::Int), T::Qty { unit, exact }) => T::Qty {
                    unit: unit.clone(),
                    exact: *exact,
                },
                (T::Qty { unit, .. }, n) | (n, T::Qty { unit, .. }) if n.is_numeric() => T::Qty {
                    unit: unit.clone(),
                    exact: true,
                },
                (T::Num(a), T::Num(b)) => T::Num(if *a == Num::Int && *b == Num::Int {
                    Num::Int
                } else {
                    Num::Decimal
                }),
                (T::Num(n), T::Zero) | (T::Zero, T::Num(n)) => T::Num(n.clone()),
                (T::Duration, n) if n.is_numeric() => T::Duration,
                (T::Money { .. }, T::Money { .. })
                | (T::Money { .. }, T::Qty { .. })
                | (T::Qty { .. }, T::Money { .. }) => {
                    self.error(span, format!("cannot multiply {l} by {r}"));
                    T::Unknown
                }
                _ => T::Unknown,
            },
            Div => match (&l, &r) {
                (T::Money { .. }, T::Money { .. })
                | (T::Qty { .. }, T::Qty { .. })
                | (T::Price { .. }, T::Price { .. }) => T::Num(Num::Decimal),
                (T::Money { cur, .. }, T::Qty { unit, .. }) => T::Price {
                    cur: cur.clone(),
                    per: unit.clone(),
                    exact: true,
                },
                (T::Money { cur, .. }, n) if n.is_numeric() => T::Money {
                    cur: cur.clone(),
                    exact: true,
                },
                (T::Price { cur, per, .. }, n) if n.is_numeric() => T::Price {
                    cur: cur.clone(),
                    per: per.clone(),
                    exact: true,
                },
                (T::Qty { unit, .. }, n) if n.is_numeric() => T::Qty {
                    unit: unit.clone(),
                    exact: true,
                },
                (T::Num(_), T::Num(_)) | (T::Zero, _) => T::Num(Num::Decimal),
                _ => T::Unknown,
            },
            _ => T::Unknown,
        }
    }

    // ---- match ------------------------------------------------------------------------

    fn match_expr(&mut self, subject: &Expr, arms: &[Arm], span: Span) -> T {
        let st = self.infer(subject);
        let mut covered: HashSet<String> = HashSet::new();
        let mut catch_all = false;
        let mut has_none = false;
        let mut out = T::Unknown;
        for arm in arms {
            self.push();
            for p in &arm.patterns {
                match p {
                    Pattern::Wildcard => catch_all = true,
                    Pattern::Lit(Lit::None) => has_none = true,
                    Pattern::Lit(_) => {}
                    Pattern::Name(n) => {
                        if n.chars().next().map_or(false, char::is_uppercase) {
                            covered.insert(n.clone());
                        } else {
                            catch_all = true;
                            self.bind(n, st.unopt().clone());
                        }
                    }
                    Pattern::Ctor { name, args } => {
                        covered.insert(name.clone());
                        self.bind_ctor(name, args, &st);
                    }
                }
            }
            let t = self.infer(&arm.body);
            self.pop();
            out = unify(out, t);
        }
        if !catch_all {
            let mut missing: Vec<String> = Vec::new();
            if let T::Enum { variants, .. } = st.unopt() {
                missing.extend(variants.iter().filter(|v| !covered.contains(*v)).cloned());
            }
            if matches!(st, T::Opt(_)) && !has_none && matches!(st.unopt(), T::Enum { .. }) {
                missing.push("none".into());
            }
            if !missing.is_empty() {
                self.error(
                    span,
                    format!("non-exhaustive match: missing {}", missing.join(", ")),
                );
            }
        }
        out
    }

    fn bind_ctor(&mut self, name: &str, args: &[Pattern], subject: &T) {
        if name == "some" {
            if let Some(Pattern::Name(b)) = args.first() {
                self.bind(b, subject.unopt().clone());
            }
            return;
        }
        if self.m.entity(name).is_some() {
            if let Some(Pattern::Name(b)) = args.first() {
                self.bind(b, T::Ent(name.into()));
            }
            return;
        }
        if let Some(v) = self.m.variant(name) {
            let fields = v.fields.clone();
            for (i, a) in args.iter().enumerate() {
                if let Pattern::Name(b) = a {
                    let t = fields
                        .get(i)
                        .map(|(_, t)| self.decl_ty(t, &["self".into()]))
                        .unwrap_or(T::Unknown);
                    self.bind(b, t);
                }
            }
        }
    }

    // ---- calls ------------------------------------------------------------------------

    fn kwarg<'a>(args: &'a [Arg], name: &str) -> Option<&'a Expr> {
        args.iter()
            .find(|a| a.name.as_deref() == Some(name))
            .map(|a| &a.value)
    }

    fn call(&mut self, callee: &Expr, args: &[Arg], span: Span) -> T {
        match &callee.kind {
            ExprKind::Name(n) => self.call_name(n, args, span),
            ExprKind::Field {
                base,
                name,
                optional,
            } => {
                let bt = self.infer(base);
                let bpath = self.path(base);
                if matches!(bt, T::Opt(_)) && !optional && self.in_fstring == 0 {
                    let narrowed = bpath
                        .as_ref()
                        .map_or(false, |p| self.narrowed.contains(&path_str(p)));
                    if !narrowed {
                        self.error(
                            span,
                            format!(
                                "`{}` is optional: use `?.` (or check it is not none first)",
                                print::expr(base)
                            ),
                        );
                    }
                }
                let inner = bt.unopt().clone();
                // records `from` a format: `ShopOrder.parse(text)` and `order.json()`, `x.xml()`;
                // `PosRow.parse_rows(text)` and `rows.csv()`, `rows.fixed()` for files of rows
                let format_of = |m: &'m Model, r: &str| -> Option<&'m str> {
                    m.entity(r).and_then(|e| e.format.as_deref())
                };
                let by_rows = |f: &str| matches!(f, "csv" | "fixed");
                match (&inner, name.as_str()) {
                    (T::Table(r), "parse" | "parse_rows") if format_of(self.m, r).is_some() => {
                        let r = r.clone();
                        let fmt = format_of(self.m, &r).unwrap_or_default();
                        let what = quadra_ir::formats::title(fmt);
                        if by_rows(fmt) && name == "parse" {
                            self.error(span, format!("{what} is read row by row: `{r}.parse_rows(text)`, a list of `{r}`"));
                        } else if !by_rows(fmt) && name == "parse_rows" {
                            self.error(span, format!("{what} is one document: `{r}.parse(text)`"));
                        }
                        match args {
                            [a] => {
                                let t = self.infer(&a.value);
                                if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
                                    self.error(
                                        a.value.span,
                                        format!("`{r}.{name}` reads text: the document"),
                                    );
                                }
                            }
                            _ => self.error(
                                span,
                                format!(
                                    "`{r}.{name}` takes one argument, the text of the document"
                                ),
                            ),
                        }
                        return if by_rows(fmt) {
                            T::list(T::Ent(r))
                        } else {
                            T::Ent(r)
                        };
                    }
                    (T::Ent(r), "json" | "xml" | "csv" | "fixed")
                        if format_of(self.m, r).is_some() =>
                    {
                        let fmt = format_of(self.m, r).unwrap_or_default();
                        if by_rows(fmt) {
                            self.error(span, format!("`{r}` is a row of {}: the file is written from the list of its rows, `rows.{fmt}()`", quadra_ir::formats::title(fmt)));
                        } else if name != fmt {
                            self.error(
                                span,
                                format!(
                                    "`{r}` is a record `from {fmt}`: it is written with `{fmt}()`"
                                ),
                            );
                        }
                        if !args.is_empty() {
                            self.error(span, format!("`{name}()` takes no argument"));
                        }
                        return T::text();
                    }
                    (T::Coll { elem, .. }, "csv" | "fixed") => {
                        if let T::Ent(r) = elem.unopt() {
                            if let Some(fmt) = format_of(self.m, r) {
                                if fmt != name {
                                    self.error(span, format!("`{r}` is a record `from {fmt}`: `{name}()` writes the rows of a record `from {name}`"));
                                }
                                if !args.is_empty() {
                                    self.error(span, format!("`{name}()` takes no argument"));
                                }
                                // the header of an empty list: the runtime cannot see its type
                                self.row_records.insert(callee.span, r.clone());
                                return T::text();
                            }
                        }
                    }
                    _ => {}
                }
                let bpath = bpath
                    .or_else(|| Some(vec![format!("#{}", u32::from(base.span.range.start()))]));
                self.record_access(&inner, name, callee.span);
                let t = self.method(&inner, bpath, name, args, span);
                if *optional && matches!(bt, T::Opt(_)) {
                    T::opt(t)
                } else {
                    t
                }
            }
            _ => {
                self.infer(callee);
                self.args(args);
                T::Unknown
            }
        }
    }

    fn args(&mut self, args: &[Arg]) -> Vec<T> {
        args.iter().map(|a| self.infer(&a.value)).collect()
    }

    fn call_name(&mut self, n: &str, args: &[Arg], span: Span) -> T {
        // local function (fn defined inside a reaction)
        if let Some(f) = self.local(n).and_then(|l| l.func.clone()) {
            return self.call_fn(&f, args);
        }
        match n {
            // the request of an `outbound … as e:` block (`check_outbounds`)
            "post" | "put" | "patch" | "delete" | "email"
                if self.context.starts_with("outbound ") =>
            {
                for a in args {
                    let t = self.infer(&a.value);
                    let ok = match (n, a.name.as_deref()) {
                        // the body: a record `from json` or `from xml` (sent as such) or text
                        (_, Some("body")) | ("post" | "put" | "patch", None)
                            if args.iter().position(|x| std::ptr::eq(x, a)) == Some(1)
                                || a.name.as_deref() == Some("body") =>
                        {
                            match t.unopt() {
                                T::Ent(r) => self.m.entity(r).is_some_and(|e| {
                                    matches!(e.format.as_deref(), Some("json" | "xml"))
                                }),
                                T::Text(_) | T::Unknown => true,
                                _ => false,
                            }
                        }
                        _ => matches!(t.unopt(), T::Text(_) | T::Unknown),
                    };
                    if !ok {
                        self.error(a.value.span, format!("`{n}(…)`: the body is a record `from json` or `from xml`, or text; the others are text"));
                    }
                }
                return T::Unknown;
            }
            // what `publish` generates
            "__from" | "__changed" => {
                for a in args {
                    self.infer(&a.value);
                }
                return T::Bool;
            }
            "now" | "today" => {
                if self.in_formula {
                    self.error(span, format!("{n}() is not allowed in a formula: formulas must give the same result every time (use `default` or an action)"));
                }
                if n == "today" {
                    // today(tz): the date in another zone than the project's
                    if let Some(a) = args.first() {
                        self.zone_arg(&a.value);
                    }
                    if args.len() > 1 {
                        self.error(span, "today() takes at most one argument, the time zone (`today(\"Europe/Rome\")`)");
                    }
                } else if !args.is_empty() {
                    self.error(span, "now() takes no argument: it is an instant, the same in every zone (`now().in(tz)` reads it in one)");
                }
                return if n == "now" { T::DateTime } else { T::Date };
            }
            "datetime" => {
                // datetime(date, "09:00", tz?): the instant of a wall-clock time in a zone
                let date = args.first().map(|a| self.infer(&a.value));
                if !matches!(
                    date.as_ref().map(|t| t.unopt()),
                    Some(T::Date) | Some(T::Unknown)
                ) {
                    self.error(span, "datetime() takes a date, a time of day and optionally a time zone: `datetime(due, \"09:00\")`");
                }
                match args.get(1) {
                    Some(a) => {
                        let t = self.infer(&a.value);
                        if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
                            self.error(a.value.span, "the time of day is text: `\"09:00\"`");
                        }
                        if let ExprKind::Lit(Lit::Str(s)) = &a.value.kind {
                            if chrono::NaiveTime::parse_from_str(s, "%H:%M").is_err()
                                && chrono::NaiveTime::parse_from_str(s, "%H:%M:%S").is_err()
                            {
                                self.error(
                                    a.value.span,
                                    format!("`{s}` is not a time of day (`09:00`, `17:30:15`)"),
                                );
                            }
                        }
                    }
                    None => self.error(
                        span,
                        "datetime() needs the time of day: `datetime(due, \"09:00\")`",
                    ),
                }
                if let Some(a) = args.get(2) {
                    self.zone_arg(&a.value);
                }
                return T::DateTime;
            }
            "convert" => {
                let ts = self.args(args);
                if self.in_formula
                    && Self::kwarg(args, "on").is_none()
                    && Self::kwarg(args, "rate").is_none()
                {
                    self.error(span, "convert() in a formula needs `on=` or `rate=`: the exchange rate of today is not deterministic");
                }
                let to = Self::kwarg(args, "to")
                    .map(|e| self.unit_expr(e))
                    .unwrap_or(Unit::Unknown);
                return match ts.first().map(|t| t.unopt().clone()) {
                    Some(T::Money { .. }) => T::Money {
                        cur: to,
                        exact: true,
                    },
                    Some(T::Price { per, .. }) => T::Price {
                        cur: to,
                        per,
                        exact: true,
                    },
                    _ => T::Unknown,
                };
            }
            "why" => {
                self.args(args);
                return T::text();
            }
            "add" if self.local("__ledger").is_some() => {
                let ledger = match self.local("__ledger").map(|l| l.ty.clone()) {
                    Some(T::Ent(l)) => l,
                    _ => String::new(),
                };
                // the ledger as a whole depends on what is posted to it
                let saved = self.current.replace((ledger.clone(), "*".into()));
                for a in args {
                    self.infer(&a.value);
                }
                self.current = saved;
                self.check_ledger_add(args, span);
                return T::Unknown;
            }
            "login" | "affected" | "count_distinct" | "add" | "provisional" | "sum" => {
                let ts = self.args(args);
                return match n {
                    "affected" | "count_distinct" => T::Num(Num::Int),
                    "provisional" | "sum" => ts.first().cloned().unwrap_or(T::Unknown),
                    _ => T::Unknown,
                };
            }
            "Qty" => {
                self.args(args);
                let u = args
                    .get(1)
                    .map(|a| self.unit_expr(&a.value))
                    .unwrap_or(Unit::Unknown);
                return T::Qty {
                    unit: u,
                    exact: false,
                };
            }
            "Price" => {
                let ts = self.args(args);
                let per = Self::kwarg(args, "per")
                    .map(|e| self.unit_expr(e))
                    .unwrap_or(Unit::Unknown);
                let cur = match ts.first().map(|t| t.unopt().clone()) {
                    Some(T::Money { cur, .. }) => cur,
                    _ => Unit::Unknown,
                };
                return T::Price {
                    cur,
                    per,
                    exact: false,
                };
            }
            "DateRange" => {
                self.args(args);
                return T::DateRange;
            }
            _ => {}
        }
        if let Some(f) = self.m.function(n) {
            return self.call_fn(f, args);
        }
        if let Some(metric) = self.m.metrics.iter().find(|x| x.name == n && !x.control) {
            self.record_read("metric", n, span);
            let f = FnDef {
                name: n.to_string(),
                params: metric.params.clone(),
                ret: metric.ret.clone(),
                body: None,
                builtin: false,
                span: metric.span,
            };
            return self.call_fn(&f, args);
        }
        if let Some(ev) = self.m.event(n) {
            let fields: Vec<String> = ev.fields.iter().map(|(f, _)| f.clone()).collect();
            for a in args {
                self.infer(&a.value);
                if let Some(name) = &a.name {
                    if !fields.contains(name) {
                        self.error(a.value.span, format!("event `{n}` has no field `{name}`"));
                    }
                }
            }
            return T::Event(n.to_string());
        }
        if let Some(e) = self.m.entity(n) {
            return self.construct(e, args, span);
        }
        if let Some(v) = self.m.variant(n) {
            return self.variant_ctor(
                n,
                &v.fields.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
                args,
                span,
            );
        }
        if let Some(en) = self.entity {
            if let Some(a) = en.actions.iter().find(|a| a.name == n) {
                let ret = a.ret.clone();
                self.args(args);
                return ret
                    .map(|t| self.decl_ty(&t, &["self".into()]))
                    .unwrap_or(T::Unknown);
            }
            if let Some(f) = en.methods.iter().find(|f| f.name == n) {
                return self.call_fn(f, args);
            }
        }
        self.name(n, span);
        self.args(args);
        T::Unknown
    }

    fn call_fn(&mut self, f: &FnDef, args: &[Arg]) -> T {
        let ts = self.args(args);
        let Some(ret) = &f.ret else { return T::Unknown };
        let local = {
            let saved = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
            for p in &f.params {
                self.bind(&p.name, T::Unknown);
            }
            let t = self.decl_ty(ret, &["self".into()]);
            self.scopes = saved;
            t
        };
        // substitute parameter roots with the arguments
        let mut subst: HashMap<String, (Option<Vec<String>>, T)> = HashMap::new();
        for (i, p) in f.params.iter().enumerate() {
            let arg = args
                .iter()
                .find(|a| a.name.as_deref() == Some(&p.name))
                .or_else(|| args.get(i).filter(|a| a.name.is_none()));
            if let Some(a) = arg {
                let at = ts
                    .get(args.iter().position(|x| std::ptr::eq(x, a)).unwrap())
                    .cloned()
                    .unwrap_or(T::Unknown);
                subst.insert(p.name.clone(), (self.path(&a.value), at));
            }
        }
        map_units(local, &|u| match u {
            Unit::Path(p) => match subst.get(&p[0]) {
                Some((_, T::Money { cur, .. })) if p.len() == 2 && p[1] == "currency" => {
                    cur.clone()
                }
                Some((Some(ap), _)) => {
                    let mut out = ap.clone();
                    out.extend(p[1..].iter().cloned());
                    Unit::Path(out)
                }
                Some(_) => Unit::Unknown,
                None => Unit::Path(p),
            },
            other => other,
        })
    }

    fn unit_expr(&mut self, e: &Expr) -> Unit {
        match &e.kind {
            ExprKind::Name(n) if self.currencies.contains(n) || self.units.contains_key(n) => {
                Unit::Const(n.clone())
            }
            _ => self.path(e).map(Unit::Path).unwrap_or(Unit::Unknown),
        }
    }

    fn construct(&mut self, e: &'m Entity, args: &[Arg], span: Span) -> T {
        for a in args {
            if let Some(n) = &a.name {
                self.record_write(&e.name, n, a.value.span);
            }
            let vt = self.infer(&a.value);
            let Some(name) = &a.name else {
                self.error(
                    a.value.span,
                    format!(
                        "`{}(…)` takes named arguments: `{}(field=value)`",
                        e.name, e.name
                    ),
                );
                continue;
            };
            match self.m.find_field(e, name) {
                None => {
                    self.error(a.value.span, format!("`{}` has no field `{name}`", e.name));
                }
                Some(f) if f.kind == FieldKind::Formula => {
                    self.error(
                        a.value.span,
                        format!("`{name}` is a formula: it cannot be set"),
                    );
                }
                Some(_) if matches!(&a.value.kind, ExprKind::Name(n) if self.m.initial_state(e, name).map_or(false, |i| &i != n) && e.flows.iter().any(|fl| fl.field == *name && fl.transitions.iter().any(|t| t.to == *n || t.from.contains(n)))) =>
                {
                    let initial = self.m.initial_state(e, name).unwrap_or_default();
                    self.error(a.value.span, format!("`{}.{name}` changes only through its flow: a new {} starts in {initial}", e.name, e.name));
                }
                Some(f) => {
                    if let Some(ty) = &f.ty {
                        let saved_entity = self.entity;
                        let saved_scopes =
                            std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
                        self.entity = Some(e);
                        let d = self.decl_ty_field(ty, f.per_company);
                        self.entity = saved_entity;
                        self.scopes = saved_scopes;
                        let d = self.rebase(d, None, None);
                        self.assign(&d, &vt, a.value.span, "field");
                    }
                }
            }
        }
        let _ = span;
        T::Ent(e.name.clone())
    }

    /// A format spec in an f-string fits the value: decimals for numbers, amounts, prices and
    /// quantities, zero padding for plain numbers.
    fn check_format(&mut self, t: &T, spec: &FormatSpec, span: Span) {
        let t = t.unopt();
        let plain = matches!(t, T::Num(_) | T::Zero | T::Unknown);
        let measure = plain || matches!(t, T::Money { .. } | T::Price { .. } | T::Qty { .. });
        if let (Some(p), false) = (spec.precision, measure) {
            self.error(span, format!("the format `.{p}` applies to numbers, amounts, prices and quantities, not to {t}"));
        }
        if spec.zero && !plain {
            self.error(
                span,
                format!(
                    "`0` padding applies to numbers, not to {t}: use a width (`{{x:{}}}`)",
                    spec.width
                ),
            );
        }
    }

    /// `Discount(off=10%)` or `Fixed(8.00 EUR)`: a variant with data, its fields by name or in
    /// declared order.
    fn variant_ctor(&mut self, n: &str, fields: &[String], args: &[Arg], span: Span) -> T {
        self.args(args);
        if fields.is_empty() {
            self.error(
                span,
                format!("`{n}` has no fields: write `{n}` without parentheses"),
            );
            return T::Variant(n.to_string());
        }
        let positional = args.iter().filter(|a| a.name.is_none()).count();
        if positional > fields.len() {
            self.error(
                span,
                format!("`{n}` has {} field(s): {}", fields.len(), fields.join(", ")),
            );
        }
        for a in args {
            if let Some(name) = &a.name {
                if !fields.contains(name) {
                    self.error(a.value.span, format!("`{n}` has no field `{name}`"));
                }
            }
        }
        for (i, f) in fields.iter().enumerate() {
            let named = args.iter().any(|a| a.name.as_deref() == Some(f.as_str()));
            if !named && i >= positional {
                self.error(span, format!("`{n}` needs `{f}`"));
            }
        }
        T::Variant(n.to_string())
    }

    fn check_ledger_add(&mut self, args: &[Arg], span: Span) {
        let Some(T::Ent(ledger)) = self.local("__ledger").map(|l| l.ty.clone()) else {
            return;
        };
        let Some(l) = self.m.entity(&ledger) else {
            return;
        };
        for a in args {
            match &a.name {
                Some(n) if l.field(n).is_some() || n == "date" => {}
                Some(n) => self.error(
                    a.value.span,
                    format!("ledger `{ledger}` has no column `{n}`"),
                ),
                None => self.error(
                    span,
                    "add() takes named columns: add(product=…, qty=…)".to_string(),
                ),
            }
        }
    }

    // ---- methods ----------------------------------------------------------------------

    fn lambda(
        &mut self,
        args: &[Arg],
        idx: usize,
        name: Option<&str>,
        params: &[T],
        facts: &[Fact],
    ) -> T {
        let arg = match name {
            Some(n) => args.iter().find(|a| a.name.as_deref() == Some(n)),
            None => args.iter().filter(|a| a.name.is_none()).nth(idx),
        };
        let Some(arg) = arg else { return T::Unknown };
        match &arg.value.kind {
            ExprKind::Lambda { params: ps, body } => {
                self.push();
                for (i, p) in ps.iter().enumerate() {
                    self.bind(p, params.get(i).cloned().unwrap_or(T::Unknown));
                    if i == 0 && !facts.is_empty() {
                        self.facts.insert(p.clone(), facts.to_vec());
                    }
                }
                let t = self.infer(body);
                for p in ps {
                    self.facts.remove(p);
                }
                self.pop();
                t
            }
            _ => self.infer(&arg.value),
        }
    }

    /// Facts `x.rel == target` from the conjuncts of a `where` lambda.
    fn where_facts(&self, args: &[Arg]) -> Vec<Fact> {
        let Some(Arg {
            value:
                Expr {
                    kind: ExprKind::Lambda { params, body },
                    ..
                },
            ..
        }) = args.first()
        else {
            return Vec::new();
        };
        let Some(p) = params.first() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![body.as_ref()];
        while let Some(e) = stack.pop() {
            match &e.kind {
                ExprKind::Binary {
                    op: BinOp::And,
                    lhs,
                    rhs,
                } => {
                    stack.push(lhs);
                    stack.push(rhs);
                }
                ExprKind::Binary {
                    op: BinOp::Eq,
                    lhs,
                    rhs,
                } => {
                    for (a, b) in [(lhs, rhs), (rhs, lhs)] {
                        if let Some(rel) = rel_path(a, p) {
                            if rel_path(b, p).is_none() {
                                if let Some(target) = self.path(b) {
                                    out.push(Fact { rel, target });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// A time zone argument: text, and a known zone when it is written out.
    fn zone_arg(&mut self, e: &Expr) {
        let t = self.infer(e);
        if !matches!(t.unopt(), T::Text(_) | T::Unknown) {
            self.error(e.span, "a time zone is text: an IANA name such as `\"Europe/Rome\"`, or a field holding one");
        }
        if let ExprKind::Lit(Lit::Str(s)) = &e.kind {
            if !quadra_sema::is_timezone(s) {
                self.error(e.span, format!("`{s}` is not a time zone (an IANA name such as `Europe/Rome`, `America/New_York` or `UTC`)"));
            }
        }
    }

    fn method(
        &mut self,
        base: &T,
        bpath: Option<Vec<String>>,
        name: &str,
        args: &[Arg],
        span: Span,
    ) -> T {
        // collection-like receivers
        let coll = match base {
            T::Coll { elem, set, facts } => Some(((**elem).clone(), *set, facts.clone())),
            T::Table(n) => Some((T::Ent(n.clone()), false, Vec::new())),
            _ => None,
        };
        if let Some((elem, set, facts)) = coll {
            let is_table = matches!(base, T::Table(_));
            return self.coll_method(elem, set, facts, is_table, name, args, span);
        }
        match base {
            T::Ent(en) => {
                let Some(e) = self.m.entity(en) else {
                    self.args(args);
                    return T::Unknown;
                };
                if let Some(a) = e.actions.iter().find(|a| a.name == name) {
                    let ret = a.ret.clone();
                    self.args(args);
                    return ret
                        .map(|t| {
                            let d = self.decl_ty(&t, &["self".into()]);
                            self.rebase(d, bpath.as_deref(), None)
                        })
                        .unwrap_or(T::Unknown);
                }
                if let Some(f) = e.methods.iter().find(|f| f.name == name) {
                    let f = f.clone();
                    let saved = self.entity;
                    let t = self.call_fn(&f, args);
                    self.entity = saved;
                    return self.rebase(t, bpath.as_deref(), None);
                }
                if self.is_flow_action(e, name) {
                    self.args(args);
                    return T::Unknown;
                }
                match name {
                    "copy" => {
                        let en = en.clone();
                        if let Some(ent) = self.m.entity(&en) {
                            self.construct(ent, args, span);
                        }
                        return T::Ent(en);
                    }
                    "closure" => {
                        let elem = T::Ent(en.clone());
                        let r = self.lambda(args, 0, None, &[elem.clone()], &[]);
                        let inner = match r.unopt() {
                            T::Coll { elem, .. } => (**elem).clone(),
                            other => other.clone(),
                        };
                        return T::Coll {
                            elem: Box::new(inner),
                            set: true,
                            facts: Vec::new(),
                        };
                    }
                    "as_of" | "explain" => {
                        self.args(args);
                        let how = if name == "as_of" {
                            "read the history through the API (`GET /v1/records/{entity}/{id}/history`)"
                        } else {
                            "use `why(record)`"
                        };
                        self.error(
                            span,
                            format!("`{name}` is not available in expressions: {how}"),
                        );
                        return T::Unknown;
                    }
                    _ => {}
                }
                let en = en.clone();
                self.args(args);
                if e.kind != EntityKind::Trait {
                    self.error(span, format!("`{en}` has no action or method `{name}`"));
                }
                T::Unknown
            }
            T::Money { cur, exact } => {
                self.args(args);
                match name {
                    "round" => T::Money {
                        cur: cur.clone(),
                        exact: false,
                    },
                    "abs" => T::Money {
                        cur: cur.clone(),
                        exact: *exact,
                    },
                    _ => self.unknown_method(base, name, span),
                }
            }
            T::Price { cur, per, exact } => match name {
                "round" => {
                    self.args(args);
                    T::Price {
                        cur: cur.clone(),
                        per: per.clone(),
                        exact: false,
                    }
                }
                "to" => {
                    self.args(args);
                    let u = args
                        .first()
                        .map(|a| self.unit_expr(&a.value))
                        .unwrap_or(Unit::Unknown);
                    T::Price {
                        cur: cur.clone(),
                        per: u,
                        exact: *exact,
                    }
                }
                "abs" => base.clone(),
                _ => self.unknown_method(base, name, span),
            },
            T::Qty { unit, exact } => match name {
                "to" => {
                    self.args(args);
                    let u = args
                        .first()
                        .map(|a| self.unit_expr(&a.value))
                        .unwrap_or(Unit::Unknown);
                    if let (Unit::Const(a), Unit::Const(b)) = (unit, &u) {
                        if self.units.get(a) != self.units.get(b) {
                            self.error(
                                span,
                                format!("cannot convert {base} to {b}: different kinds of unit"),
                            );
                        }
                    }
                    T::Qty {
                        unit: u,
                        exact: *exact,
                    }
                }
                "round" | "round_up" => {
                    self.args(args);
                    T::Qty {
                        unit: unit.clone(),
                        exact: false,
                    }
                }
                "abs" => base.clone(),
                _ => self.unknown_method(base, name, span),
            },
            T::Duration => {
                self.args(args);
                match name {
                    "to" => T::Qty {
                        unit: args
                            .first()
                            .map(|a| self.unit_expr(&a.value))
                            .unwrap_or(Unit::Unknown),
                        exact: false,
                    },
                    _ => T::Duration,
                }
            }
            T::Num(_) | T::Zero => {
                self.args(args);
                match name {
                    "as_percent" => T::Num(Num::Percent),
                    "abs" | "round" => base.clone(),
                    _ => self.unknown_method(base, name, span),
                }
            }
            T::Text(_) => {
                self.args(args);
                match name {
                    "len" | "sequence_part" => T::Num(Num::Int),
                    "starts_with" | "contains" | "ends_with" => T::Bool,
                    "upper" | "lower" | "trim" | "take" | "initials" => T::text(),
                    _ => self.unknown_method(base, name, span),
                }
            }
            T::DateTime if name == "in" => {
                match args {
                    [a] => self.zone_arg(&a.value),
                    _ => self.error(span, "`in` takes the time zone: `dt.in(\"Europe/Rome\").hour`, `dt.in(customer.timezone).date`"),
                }
                T::Ent("ZonedTime".into())
            }
            T::Date | T::DateTime => {
                self.args(args);
                match name {
                    "end_of_month" | "start_of_month" => base.clone(),
                    _ => self.unknown_method(base, name, span),
                }
            }
            T::DateRange => {
                self.args(args);
                match name {
                    "contains" => T::Bool,
                    _ => self.unknown_method(base, name, span),
                }
            }
            _ => {
                self.args(args);
                T::Unknown
            }
        }
    }

    fn unknown_method(&mut self, base: &T, name: &str, span: Span) -> T {
        self.error(span, format!("{base} has no method `{name}`"));
        T::Unknown
    }

    #[allow(clippy::too_many_arguments)]
    fn coll_method(
        &mut self,
        elem: T,
        set: bool,
        facts: Vec<Fact>,
        is_table: bool,
        name: &str,
        args: &[Arg],
        span: Span,
    ) -> T {
        let same = |facts: Vec<Fact>| T::Coll {
            elem: Box::new(elem.clone()),
            set,
            facts,
        };
        match name {
            "where" => {
                self.lambda(args, 0, None, &[elem.clone()], &facts);
                let mut f = facts.clone();
                f.extend(self.where_facts(args));
                same(f)
            }
            "all" if is_table && args.is_empty() => same(facts),
            "all" | "any" | "none" => {
                if !args.is_empty() {
                    self.lambda(args, 0, None, &[elem.clone()], &facts);
                }
                T::Bool
            }
            "count" => {
                if !args.is_empty() {
                    self.lambda(args, 0, None, &[elem.clone()], &facts);
                }
                T::Num(Num::Int)
            }
            "contains" => {
                self.args(args);
                T::Bool
            }
            "is_contiguous" => T::Bool,
            "distinct" => same(facts),
            "first" | "last" => {
                // `first(n)` / `last(n)`: the first or last n elements
                let counted = matches!(args, [Arg { name: None, value, .. }] if !matches!(value.kind, ExprKind::Lambda { .. }));
                if counted {
                    let n = self.infer(&args[0].value);
                    if !matches!(n.unopt(), T::Num(Num::Int) | T::Unknown | T::Zero) {
                        self.error(span, format!("{name}(n) takes a whole number, not {n}"));
                    }
                    return T::Coll {
                        elem: Box::new(elem),
                        set: false,
                        facts,
                    };
                }
                if !args.is_empty() {
                    self.lambda(args, 0, None, &[elem.clone()], &facts);
                }
                T::opt(elem)
            }
            "single" => {
                if !args.is_empty() {
                    self.lambda(args, 0, None, &[elem.clone()], &facts);
                }
                elem
            }
            "find" | "last_by" => {
                self.lambda(args, 0, None, &[elem.clone()], &facts);
                T::opt(elem)
            }
            "sort_by" => {
                self.lambda(args, 0, None, &[elem.clone()], &facts);
                T::list(elem)
            }
            "max" | "min" => {
                let r = if args.is_empty() {
                    elem.clone()
                } else {
                    self.lambda(args, 0, None, &[elem.clone()], &facts)
                };
                T::opt(r.unopt().clone())
            }
            "avg" => {
                // the mean of the values that are not none; none for no value
                let r = if args.is_empty() {
                    elem.clone()
                } else {
                    self.lambda(args, 0, None, &[elem.clone()], &facts)
                };
                let param = match args.first().map(|a| &a.value.kind) {
                    Some(ExprKind::Lambda { params, .. }) => params.first().cloned(),
                    _ => None,
                };
                if let Some(p) = param {
                    let depends = |u: &Unit| u.root() == Some(p.as_str());
                    match &r {
                        T::Money { cur, .. } if depends(cur) => self.error(span, format!("cannot average {r}: amounts in different currencies (each `{p}` has its own); convert them first")),
                        T::Qty { unit, .. } if depends(unit) => self.error(span, format!("cannot average {r}: quantities in different units (each `{p}` has its own); convert them with .to() first")),
                        _ => {}
                    }
                }
                match r.unopt() {
                    T::Num(Num::Int) | T::Zero => T::opt(T::Num(Num::Decimal)),
                    T::Money { cur, .. } => T::opt(T::Money {
                        cur: cur.clone(),
                        exact: false,
                    }),
                    T::Qty { unit, .. } => T::opt(T::Qty {
                        unit: unit.clone(),
                        exact: false,
                    }),
                    T::Price { cur, per, .. } => T::opt(T::Price {
                        cur: cur.clone(),
                        per: per.clone(),
                        exact: false,
                    }),
                    T::Num(n) => T::opt(T::Num(n.clone())),
                    T::Duration => T::opt(T::Duration),
                    T::Unknown => T::Unknown,
                    other => {
                        let other = other.clone();
                        self.error(span, format!("cannot average {other}: numbers, amounts, quantities, prices or durations only"));
                        T::Unknown
                    }
                }
            }
            "map" => {
                let r = self.lambda(args, 0, None, &[elem.clone()], &facts);
                T::list(r)
            }
            "flat_map" => {
                let r = self.lambda(args, 0, None, &[elem.clone()], &facts);
                let inner = match r {
                    T::Coll { elem, .. } => *elem,
                    T::Opt(i) => *i,
                    other => other,
                };
                T::list(inner)
            }
            "sum" => {
                let r = if args.is_empty() {
                    elem.clone()
                } else {
                    self.lambda(args, 0, None, &[elem.clone()], &facts)
                };
                // the result's unit must not depend on the element
                let param = match args.first().map(|a| &a.value.kind) {
                    Some(ExprKind::Lambda { params, .. }) => params.first().cloned(),
                    _ => None,
                };
                if let Some(p) = param {
                    let depends = |u: &Unit| u.root() == Some(p.as_str());
                    match &r {
                        T::Money { cur, .. } if depends(cur) => {
                            self.error(span, format!("cannot sum {r}: amounts in different currencies (each `{p}` has its own); convert them first"));
                        }
                        T::Qty { unit, .. } if depends(unit) => {
                            self.error(span, format!("cannot sum {r}: quantities in different units (each `{p}` has its own); convert them with .to() first"));
                        }
                        _ => {}
                    }
                }
                if matches!(r.unopt(), T::Money { .. } | T::Qty { .. } | T::Price { .. }) {
                    self.sum_types.insert(span, r.unopt().clone());
                }
                match r {
                    T::Num(Num::Int) => T::Num(Num::Int),
                    other => other,
                }
            }
            "group_by" => {
                // one or two keys: groups `(key, items)` or `(key, key2, items)`
                let positional = args.iter().filter(|a| a.name.is_none()).count();
                if positional == 0 || positional > 2 || args.iter().any(|a| a.name.is_some()) {
                    self.error(span, "group_by takes one or two keys: group_by(x => key) or group_by(x => key, x => key2)");
                }
                let k = self.lambda(args, 0, None, &[elem.clone()], &facts);
                let items = T::Coll {
                    elem: Box::new(elem.clone()),
                    set: false,
                    facts: facts.clone(),
                };
                let mut fields = vec![(Some("key".into()), k)];
                if positional == 2 {
                    let k2 = self.lambda(args, 1, None, &[elem.clone()], &facts);
                    fields.push((Some("key2".into()), k2));
                }
                fields.push((Some("items".into()), items));
                T::list(T::Tuple(fields))
            }
            "allocate" => {
                let total = args
                    .first()
                    .map(|a| self.infer(&a.value))
                    .unwrap_or(T::Unknown);
                self.lambda(args, 0, Some("by"), &[elem.clone()], &facts);
                let share = total.unopt().clone().with_exact(false);
                T::list(T::Tuple(vec![
                    (Some("item".into()), elem.clone()),
                    (Some("share".into()), share),
                ]))
            }
            _ => {
                self.args(args);
                self.error(span, format!("collections have no method `{name}`"));
                T::Unknown
            }
        }
    }
}

const BUILTIN_FNS: &[&str] = &[
    "convert",
    "today",
    "now",
    "datetime",
    "why",
    "login",
    "affected",
    "count_distinct",
    "add",
    "provisional",
    "sum",
    "Qty",
    "Price",
    "DateRange",
    "some",
];

fn is_none(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Lit(Lit::None))
}

/// `x.a.b` → `[a, b]` when rooted at `param`.
fn rel_path(e: &Expr, param: &str) -> Option<Vec<String>> {
    match &e.kind {
        ExprKind::Name(n) if n == param => Some(Vec::new()),
        ExprKind::Field { base, name, .. } => {
            let mut p = rel_path(base, param)?;
            p.push(name.clone());
            Some(p)
        }
        _ => None,
    }
    .filter(|p: &Vec<String>| !p.is_empty() || matches!(e.kind, ExprKind::Name(_)))
}

/// A path relative to an instance of `e`: `payment.currency` → `[payment, currency]`.
fn entity_rel_path(x: &Expr, e: &Entity, m: &Model) -> Option<Vec<String>> {
    match &x.kind {
        ExprKind::Name(n) if m.find_field(e, n).is_some() => Some(vec![n.clone()]),
        ExprKind::Field { base, name, .. } => {
            let mut p = entity_rel_path(base, e, m)?;
            p.push(name.clone());
            Some(p)
        }
        _ => None,
    }
}

fn call_parts(e: &Expr) -> Option<(&str, &[Arg])> {
    match &e.kind {
        ExprKind::Call { callee, args } => match &callee.kind {
            ExprKind::Name(n) => Some((n.as_str(), args.as_slice())),
            _ => None,
        },
        _ => None,
    }
}

fn str_arg(args: &[Arg], name: &str) -> Option<String> {
    args.iter()
        .find(|a| a.name.as_deref() == Some(name))
        .and_then(|a| match &a.value.kind {
            ExprKind::Lit(Lit::Str(s)) => Some(s.clone()),
            _ => None,
        })
}

fn map_units(t: T, f: &dyn Fn(Unit) -> Unit) -> T {
    match t {
        T::Money { cur, exact } => T::Money { cur: f(cur), exact },
        T::Price { cur, per, exact } => T::Price {
            cur: f(cur),
            per: f(per),
            exact,
        },
        T::Qty { unit, exact } => T::Qty {
            unit: f(unit),
            exact,
        },
        T::Opt(i) => T::Opt(Box::new(map_units(*i, f))),
        T::Coll { elem, set, facts } => T::Coll {
            elem: Box::new(map_units(*elem, f)),
            set,
            facts,
        },
        T::Tuple(items) => T::Tuple(
            items
                .into_iter()
                .map(|(n, t)| (n, map_units(t, f)))
                .collect(),
        ),
        other => other,
    }
}

fn unify(a: T, b: T) -> T {
    match (a, b) {
        (T::Unknown, b) => b,
        (a, T::Unknown) => a,
        (T::None, b) => T::opt(b),
        (a, T::None) => T::opt(a),
        (T::Zero, b) => b,
        (a, T::Zero) => a,
        (T::Variant(_), b @ T::Enum { .. }) => b,
        (T::Opt(a), b) => T::opt(unify(*a, b)),
        (a, T::Opt(b)) => T::opt(unify(a, *b)),
        (T::Money { cur, exact: e1 }, T::Money { exact: e2, .. }) => T::Money {
            cur,
            exact: e1 || e2,
        },
        (
            T::Price {
                cur,
                per,
                exact: e1,
            },
            T::Price { exact: e2, .. },
        ) => T::Price {
            cur,
            per,
            exact: e1 || e2,
        },
        (a, _) => a,
    }
}

/// The kind of a value, for assignments; None when the type says too little to decide.
fn kind_of(t: &T) -> Option<&'static str> {
    Some(match t {
        T::Money { .. } => "money",
        T::Price { .. } => "price",
        T::Qty { .. } => "quantity",
        T::Num(_) => "number",
        T::Text(_) => "text",
        T::Bool => "boolean",
        T::Date | T::DateTime => "date",
        T::Duration => "duration",
        T::DateRange => "date range",
        _ => return None,
    })
}
