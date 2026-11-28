//! Editor hints about how the code meets the database (the language server and
//! `POST /v1/dev/check`): computed from the model and its schema alone, without a database.
//!
//! - **Queries** (`Entity.where/find/count/any/all/first/last/sum/min/max/sort_by…`, `for x in
//!   Entity.where(…)`): whether they run in SQL using an index, in SQL reading every row of the
//!   table (no index the schema infers covers the condition: references, `unique … when`,
//!   dates, display fields, fields compared with `==` in row rules), or in the interpreter (and
//!   why: the translator's reason, `sqlq::cond_sql`).
//! - **Row rules** (`read … where …`) that `sqlq::rules_sql` cannot translate: applied by the
//!   interpreter row by row.
//! - **Formulas that are not incremental** (`FieldMap::incremental`): computed when read, with
//!   the reason (`quadra_analysis::deps::incremental`).
//!
//! Hints have their own severity (`Severity::Hint`) and are deduplicated by place and text.
//! The values a condition compares the row with are not known here: each is replaced by a
//! value of the type of what it is compared with.

use crate::conn::Dialect;
use crate::schema::Schema;
use quadra_ir::{Arg, BinOp, EntityKind, Expr, ExprKind, Model, Stmt, StmtKind, Ty};
use quadra_runtime::query::{Binder, Frees};
use quadra_runtime::V;
use quadra_sema::{Diagnostic, Severity};
use std::collections::HashSet;
use std::rc::Rc;

/// The methods over a whole entity answered in SQL (`interp/query.rs`).
const SQL_METHODS: &[&str] = &[
    "where", "count", "any", "none", "all", "first", "last", "find", "sum", "min", "max", "last_by",
];

/// Every hint of a checked project (none when it has errors).
pub fn hints(checked: &quadra_analysis::Checked) -> Vec<Diagnostic> {
    let Some(m) = checked.model.as_ref() else {
        return Vec::new();
    };
    let schema = crate::schema::build(m, &checked.report);
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut seen: HashSet<(u32, u32, u32, String)> = HashSet::new();
    let mut push = |span: &quadra_ir::Span, message: String, out: &mut Vec<Diagnostic>| {
        let key = (
            span.file.0,
            u32::from(span.range.start()),
            u32::from(span.range.end()),
            message.clone(),
        );
        if seen.insert(key) {
            out.push(Diagnostic {
                file: span.file,
                range: span.range,
                severity: Severity::Hint,
                message,
            });
        }
    };
    // queries in the code
    let mut found: Vec<(quadra_ir::Span, String)> = Vec::new();
    let mut w = Walker {
        m,
        schema: &schema,
        out: &mut found,
        chained: HashSet::new(),
    };
    for e in &m.entities {
        for f in &e.fields {
            for x in [&f.default, &f.formula, &f.required_when]
                .into_iter()
                .flatten()
            {
                w.expr(x);
            }
            if let Some(quadra_ir::Where::Expr(x) | quadra_ir::Where::Compare(_, x)) = &f.where_ {
                w.expr(x);
            }
        }
        for c in &e.checks {
            w.expr(&c.cond);
        }
        for a in &e.actions {
            w.stmts(&a.body);
        }
        for f in &e.methods {
            w.stmts(f.body.as_deref().unwrap_or(&[]));
        }
        for p in &e.posts {
            w.stmts(&p.body);
        }
        for u in &e.uniques {
            if let Some(x) = &u.when {
                w.expr(x);
            }
        }
    }
    for f in &m.fns {
        w.stmts(f.body.as_deref().unwrap_or(&[]));
    }
    for r in &m.reactions {
        w.stmts(&r.body);
    }
    for x in &m.metrics {
        for (_, e) in x.meta.iter().chain(x.formulas.iter()) {
            w.expr(e);
        }
    }
    for b in m.migrations.iter().chain(m.changes.iter()) {
        w.stmts(&b.body);
    }
    for (span, msg) in found {
        push(&span, msg, &mut out);
    }
    // row rules the database cannot apply
    let mut i = quadra_runtime::Interp::new(m);
    for r in crate::sqlq::rules_sql(&mut i, &schema, Dialect::Sqlite) {
        if let Err(why) = &r.sql {
            push(&r.span, format!("This rule is applied by the interpreter, row by row, on every {} a query reads (not in SQL): {why}", r.entity), &mut out);
        }
    }
    // formulas computed when read
    for t in &schema.tables {
        let crate::schema::Source::Entity(ent) = &t.source else {
            continue;
        };
        let Some(e) = m.entity(ent) else { continue };
        for fm in t.fields.iter().filter(|f| f.formula && !f.incremental) {
            let Some(f) = e.fields.iter().find(|f| f.name == fm.field) else {
                continue;
            };
            let why = quadra_analysis::deps::incremental(m, ent, &fm.field)
                .err()
                .unwrap_or_default();
            push(&f.span, format!("`{ent}.{}` is computed when read and on every commit that may change it (not kept up to date by its changes): {why}", fm.field), &mut out);
        }
    }
    out.sort_by_key(|d| (d.file, d.range.start()));
    out
}

struct Walker<'a> {
    m: &'a Model,
    schema: &'a Schema,
    out: &'a mut Vec<(quadra_ir::Span, String)>,
    /// the calls of a chain already described (the inner `.where` of `….where(…).count()`)
    chained: HashSet<usize>,
}

/// `Entity.where(λ).where(μ)…`: the entity and the lambdas, with the calls of the chain.
fn chain<'e>(
    m: &Model,
    e: &'e Expr,
    calls: &mut Vec<usize>,
) -> Option<(String, Vec<(String, &'e Expr)>)> {
    match &e.kind {
        ExprKind::Name(n) => {
            let en = m.entity(n)?;
            (en.kind == EntityKind::Entity && !en.is_ledger && !en.temporary)
                .then(|| (n.clone(), Vec::new()))
        }
        ExprKind::Call { callee, args } => {
            let ExprKind::Field {
                base,
                name,
                optional: false,
            } = &callee.kind
            else {
                return None;
            };
            if name != "where" {
                return None;
            }
            let lambda = lambda_of(args)?;
            let (t, mut l) = chain(m, base, calls)?;
            calls.push(e as *const Expr as usize);
            l.push(lambda);
            Some((t, l))
        }
        _ => None,
    }
}

fn lambda_of(args: &[Arg]) -> Option<(String, &Expr)> {
    match args {
        [a] if a.name.is_none() => match &a.value.kind {
            ExprKind::Lambda { params, body } if params.len() == 1 => {
                Some((params[0].clone(), body.as_ref()))
            }
            _ => None,
        },
        _ => None,
    }
}

impl<'a> Walker<'a> {
    fn stmts(&mut self, body: &[Stmt]) {
        for s in body {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { value, .. } => self.expr(value),
            StmtKind::If {
                cond,
                then,
                otherwise,
                ..
            } => {
                self.expr(cond);
                self.stmts(then);
                if let Some(o) = otherwise {
                    self.stmts(o);
                }
            }
            StmtKind::For { iter, body, .. } => {
                self.expr(iter);
                self.stmts(body);
            }
            StmtKind::Return(Some(e))
            | StmtKind::Fail(e)
            | StmtKind::Emit(e)
            | StmtKind::Expect(e)
            | StmtKind::Clock(e)
            | StmtKind::Expr(e)
            | StmtKind::Meta { value: e, .. } => self.expr(e),
            StmtKind::Require { cond, message } => {
                self.expr(cond);
                if let Some(m) = message {
                    self.expr(m);
                }
            }
            StmtKind::ExpectError { body, .. }
            | StmtKind::As { body, .. }
            | StmtKind::Block { body, .. } => self.stmts(body),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Fn(f) => self.stmts(f.body.as_deref().unwrap_or(&[])),
            StmtKind::Migration { body, arms, .. } => {
                self.stmts(body);
                for a in arms {
                    self.expr(&a.body);
                }
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        if let ExprKind::Call { callee, args } = &e.kind {
            if !self.chained.contains(&(e as *const Expr as usize)) {
                self.query(e, callee, args);
            }
        }
        match &e.kind {
            ExprKind::FString(parts) => {
                for p in parts {
                    if let quadra_ir::FPart::Expr(x, _) = p {
                        self.expr(x);
                    }
                }
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Fail(expr)
            | ExprKind::As { body: expr, .. } => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond);
                self.expr(then);
                self.expr(otherwise);
            }
            ExprKind::Lambda { body, .. } => self.expr(body),
            ExprKind::Tuple(items) => items.iter().for_each(|(_, x)| self.expr(x)),
            ExprKind::List(items) => items.iter().for_each(|a| self.expr(&a.value)),
            ExprKind::Set(items) => items.iter().for_each(|x| self.expr(x)),
            ExprKind::When(arms) => {
                for (c, x) in arms {
                    if let Some(c) = c {
                        self.expr(c);
                    }
                    self.expr(x);
                }
            }
            ExprKind::Match { subject, arms } => {
                self.expr(subject);
                for a in arms {
                    self.expr(&a.body);
                }
            }
            ExprKind::Block(stmts) => self.stmts(stmts),
            _ => {}
        }
    }

    /// A call over a whole entity: how it runs.
    fn query(&mut self, e: &Expr, callee: &Expr, args: &[Arg]) {
        let ExprKind::Field {
            base,
            name,
            optional: false,
        } = &callee.kind
        else {
            return;
        };
        let mut calls = Vec::new();
        // `….sort_by(k).first()` / `.last()`: the sort's chain
        let (method, base, args) = match (&base.kind, name.as_str()) {
            (
                ExprKind::Call {
                    callee: c2,
                    args: a2,
                },
                "first" | "last",
            ) if args.is_empty()
                && matches!(&c2.kind, ExprKind::Field { name, .. } if name == "sort_by") =>
            {
                let ExprKind::Field { base: b2, .. } = &c2.kind else {
                    return;
                };
                calls.push(base.as_ref() as *const Expr as usize);
                ("sort_by", b2.as_ref(), a2.as_slice())
            }
            _ => (name.as_str(), base.as_ref(), args),
        };
        let (ent, lambdas) = if method == "where" {
            // the chain itself (iterated, or kept as a list)
            let Some(c) = chain(self.m, e, &mut calls) else {
                return;
            };
            c
        } else {
            let Some((t, mut l)) = chain(self.m, base, &mut calls) else {
                return;
            };
            if matches!(
                method,
                "count" | "any" | "none" | "all" | "find" | "first" | "last"
            ) {
                if let Some(x) = lambda_of(args) {
                    l.push(x);
                }
            }
            (t, l)
        };
        self.chained.extend(calls);
        let sql_method = SQL_METHODS.contains(&method) || method == "sort_by";
        // `Customer.where(…).count()`, `Order.count(…)`
        let wheres = if method == "where" {
            lambdas.len()
        } else {
            lambdas.len() - usize::from(lambdas.len() > calls_where(&lambdas, args))
        };
        let what = format!(
            "`{ent}{}{}`",
            ".where(…)".repeat(wheres),
            match method {
                "where" => String::new(),
                "sort_by" => ".sort_by(…).first()".to_string(),
                m => format!(".{m}({})", if args.is_empty() { "" } else { "…" }),
            }
        );
        let Some(table) = self.schema.entity_table(&ent) else {
            return;
        };
        let mut untranslated: Option<String> = None;
        let mut translated = 0;
        let mut index: Option<String> = None;
        for (p, body) in &lambdas {
            let frees = sample_frees(self.m, &ent, p, body);
            match crate::sqlq::cond_sql(
                Dialect::Sqlite,
                self.schema,
                self.m,
                &ent,
                &Binder::Param(p.clone()),
                body,
                &frees,
            ) {
                Ok(_) => {
                    translated += 1;
                    if index.is_none() {
                        index = indexed(self.m, self.schema, &ent, p, body);
                    }
                }
                Err(why) => {
                    untranslated.get_or_insert(why);
                }
            }
        }
        let tail = if sql_method {
            String::new()
        } else {
            format!(" `.{method}` then runs in the interpreter on the rows SQL returns.")
        };
        let msg = match (translated, untranslated) {
            (0, Some(why)) if !lambdas.is_empty() => format!("{what} runs in the interpreter over every {ent} record (the condition does not translate to SQL: {why}).{tail}"),
            (_, Some(why)) => format!("{what} runs in SQL for part of its condition; the interpreter checks the rest on the rows SQL returns ({why}).{tail}"),
            (_, None) if lambdas.is_empty() => format!("{what} runs in SQL, reading every row of `{}`.{tail}", table.name),
            (_, None) => match index {
                Some(ix) => format!("{what} runs in SQL using an index ({ix}).{tail}"),
                None => format!(
                    "{what} runs in SQL but reads every row of `{}`: no index for its condition (indexes are inferred from references, `unique … when`, dates, display fields and fields compared with `==` in row rules).{tail}",
                    table.name
                ),
            },
        };
        // what goes cold: a query that does not keep to the recent records may read the archive
        let msg = match self
            .m
            .entity(&ent)
            .and_then(|x| x.cold.as_ref())
            .and_then(|c| c.term())
        {
            Some(t)
                if !lambdas
                    .iter()
                    .any(|(_, body)| quadra_ir::print::expr(body).contains(t.after.as_str())) =>
            {
                format!(
                    "{msg} It may read the archive: closed {ent} records go cold {} (slower to read). A condition on `{}` keeps it to the recent ones; a total kept by a formula (`sum` over a record's lines) does not read them.",
                    t.text(),
                    t.after
                )
            }
            _ => msg,
        };
        self.out.push((e.span.clone(), msg));
    }
}

/// How many of the lambdas come from `.where(…)` calls (the others: the method's own).
fn calls_where(lambdas: &[(String, &Expr)], args: &[Arg]) -> usize {
    lambdas.len()
        - usize::from(lambda_of(args).is_some_and(|(p, b)| {
            lambdas
                .last()
                .is_some_and(|(q, c)| *q == p && std::ptr::eq(*c, b))
        }))
}

/// The entity and field at the end of `param.a.b` (the entity of each step through
/// references).
fn path_end(m: &Model, ent: &str, param: &str, e: &Expr) -> Option<(String, quadra_ir::Field)> {
    let mut names = Vec::new();
    let mut cur = e;
    loop {
        match &cur.kind {
            ExprKind::Field { base, name, .. } => {
                names.push(name.clone());
                cur = base;
            }
            ExprKind::Name(n) if n == param => break,
            _ => return None,
        }
    }
    names.reverse();
    let mut at = ent.to_string();
    for (k, n) in names.iter().enumerate() {
        let f = m.find_field(m.entity(&at)?, n)?.clone();
        if k + 1 == names.len() {
            return Some((at, f));
        }
        at = entity_of(m, f.ty.as_ref()?)?;
    }
    None
}

fn entity_of(m: &Model, t: &Ty) -> Option<String> {
    match t {
        Ty::Optional(i) => entity_of(m, i),
        Ty::Path { segments, .. } if segments.len() == 1 => m
            .entity(&segments[0])
            .filter(|e| e.kind == EntityKind::Entity)
            .map(|e| e.name.clone()),
        _ => None,
    }
}

/// The index a top-level conjunct of the condition can use: a reference, or a field with an
/// index, compared with a value (`==`, `in`, `is`, a range).
fn indexed(m: &Model, s: &Schema, ent: &str, param: &str, body: &Expr) -> Option<String> {
    for c in quadra_runtime::query::conjuncts(body) {
        let ExprKind::Binary { op, lhs, rhs } = &c.kind else {
            continue;
        };
        if !matches!(
            op,
            BinOp::Eq | BinOp::In | BinOp::Is | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        ) {
            continue;
        }
        for (side, other) in [(lhs, rhs), (rhs, lhs)] {
            // the other side does not read the row
            if path_end(m, ent, param, other).is_some() {
                continue;
            }
            let Some((at, f)) = path_end(m, ent, param, side) else {
                continue;
            };
            if f.ty.as_ref().and_then(|t| entity_of(m, t)).is_some() && f.formula.is_none() {
                return Some(format!("reference `{at}.{}`", f.name));
            }
            if let Some((t, c)) = s.indexed_column(&at, &f.name) {
                return Some(format!("`{}.{}`", t.name, c.name));
            }
        }
    }
    None
}

/// Values for the parts of a condition that do not read the row: each of the type of what it
/// is compared with (a text for a text field, a record for a reference…), else a number.
fn sample_frees(m: &Model, ent: &str, param: &str, body: &Expr) -> Frees {
    let mut list = Vec::new();
    quadra_runtime::query::free_exprs(m, ent, &Binder::Param(param.to_string()), body, &mut list);
    let mut f = Frees::default();
    let mut typed: Vec<(usize, V)> = Vec::new();
    fn walk(m: &Model, ent: &str, param: &str, e: &Expr, out: &mut Vec<(usize, V)>) {
        match &e.kind {
            ExprKind::Binary { op, lhs, rhs } => {
                for (a, b) in [(lhs, rhs), (rhs, lhs)] {
                    if let Some((_, fl)) = path_end(m, ent, param, a) {
                        if let Some(t) = &fl.ty {
                            let v = sample(m, t);
                            let v = if matches!(op, BinOp::In | BinOp::NotIn) {
                                V::Set(Rc::new(vec![v]))
                            } else {
                                v
                            };
                            out.push((b.as_ref() as *const Expr as usize, v));
                        }
                    }
                }
                walk(m, ent, param, lhs, out);
                walk(m, ent, param, rhs, out);
            }
            ExprKind::Unary { expr, .. } => walk(m, ent, param, expr, out),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                for x in [cond, then, otherwise] {
                    walk(m, ent, param, x, out);
                }
            }
            ExprKind::Call { callee, args } => {
                walk(m, ent, param, callee, out);
                for a in args {
                    walk(m, ent, param, &a.value, out);
                }
            }
            ExprKind::Lambda { body, .. } => walk(m, ent, param, body, out),
            _ => {}
        }
    }
    walk(m, ent, param, body, &mut typed);
    for e in list {
        let key = e as *const Expr as usize;
        let v = typed
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or(V::Int(0));
        f.insert(e, Ok(v));
    }
    f
}

/// A value of a declared type.
fn sample(m: &Model, t: &Ty) -> V {
    let d = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    match t {
        Ty::Optional(i) => sample(m, i),
        Ty::List(i) | Ty::Set(i) => V::Set(Rc::new(vec![sample(m, i)])),
        Ty::Enum(vs) => V::Variant {
            name: Rc::from(vs.first().map(|v| v.name.as_str()).unwrap_or("")),
            fields: Rc::new(Vec::new()),
        },
        Ty::Path { segments, args, .. } => {
            let last = segments.last().map(String::as_str).unwrap_or("");
            let code = |i: usize| match args.get(i) {
                Some(Ty::Path { segments, .. }) if segments.len() == 1 => segments[0].clone(),
                _ => "EUR".to_string(),
            };
            match last {
                "Int" => V::Int(0),
                "Decimal" => V::Dec(0.into()),
                "Percent" => V::Pct(0.into()),
                "Bool" => V::Bool(false),
                "Date" => V::Date(d),
                "DateTime" => V::DateTime(d.and_hms_opt(0, 0, 0).unwrap()),
                "Money" => V::Money {
                    amt: 0.into(),
                    cur: Rc::from(code(0).as_str()),
                },
                "Qty" => V::Qty {
                    v: 0.into(),
                    unit: Rc::from(code(0).as_str()),
                },
                _ => match m.entity(last) {
                    Some(e) if e.kind == EntityKind::Entity => V::Ref {
                        ent: Rc::from(e.name.as_str()),
                        id: 0,
                    },
                    _ => match m.enums.iter().find(|x| x.name == last) {
                        Some(en) => V::Variant {
                            name: Rc::from(
                                en.variants.first().map(|v| v.name.as_str()).unwrap_or(""),
                            ),
                            fields: Rc::new(Vec::new()),
                        },
                        None => V::Text(Rc::from("")),
                    },
                },
            }
        }
        _ => V::Int(0),
    }
}
