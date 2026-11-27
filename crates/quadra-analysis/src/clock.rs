//! When something of the model next changes with time alone, without reading every record.
//!
//! A formula, a check or a `becomes` condition that reads the clock is a *threshold* when every
//! read of the clock in it is a comparison of `now()` with a stored `DateTime` field of the
//! record, possibly moved by a fixed amount: `at <= now()`, `now() > sent_at + 2 hours`,
//! `due_at + 3 days < now()`. Its value changes only when `now()` passes one of those instants,
//! so the next sweep it needs is the nearest of them: a query on one column
//! (`quadra_store::ops::next_due`). Anything else that reads the clock (`today()`, `now().hour`,
//! the clock in a function, a field of another record) keeps the periodic sweep: waking early
//! costs little, late is a bug.

use crate::deps;
use quadra_ir::{BinOp, Entity, Expr, ExprKind, Model, Trigger, Ty};

/// An instant a threshold passes for each record: the value of `field` (a stored `DateTime`)
/// moved by `days` calendar days (in the project's zone, as the interpreter does) and `secs`
/// seconds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Threshold {
    pub entity: String,
    pub field: String,
    pub days: i64,
    pub secs: i64,
}

/// How a model changes with time alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// it does not read the clock
    Never,
    /// it does, in a way only a periodic sweep follows (why)
    Periodic(String),
    /// only through thresholds: the next sweep is when one of them passes
    At(Vec<Threshold>),
}

fn is_clock_call(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Call { callee, args } if args.is_empty() && matches!(&callee.kind, ExprKind::Name(n) if n == name))
}

/// Whether an expression reads the clock anywhere (conservative: a name `now` or `today`).
fn reads_clock(e: &Expr) -> bool {
    let s = format!("{:?}", e.kind);
    s.contains("Name(\"now\")") || s.contains("Name(\"today\")")
}

fn is_datetime(ty: &Option<Ty>) -> bool {
    match ty {
        Some(Ty::Path { segments, args, .. }) => {
            args.is_empty() && segments.len() == 1 && segments[0] == "DateTime"
        }
        Some(Ty::Optional(t)) => is_datetime(&Some((**t).clone())),
        _ => false,
    }
}

/// A stored `DateTime` field of the record, possibly with a fixed amount added or taken away:
/// (field, days, seconds).
fn term(m: &Model, ent: &Entity, e: &Expr) -> Option<(String, i64, i64)> {
    let field = |name: &str| -> Option<String> {
        let f = m.all_fields(ent).into_iter().find(|f| f.name == name)?;
        (f.formula.is_none() && is_datetime(&f.ty)).then(|| name.to_string())
    };
    match &e.kind {
        ExprKind::Name(n) => Some((field(n)?, 0, 0)),
        ExprKind::Field {
            base,
            name,
            optional: false,
        } if matches!(base.kind, ExprKind::SelfRef) => Some((field(name)?, 0, 0)),
        ExprKind::Binary {
            op: op @ (BinOp::Add | BinOp::Sub),
            lhs,
            rhs,
        } => {
            let (f, d, s) = term(m, ent, lhs)?;
            let ExprKind::Quantity {
                value,
                unit,
                per: None,
            } = &rhs.kind
            else {
                return None;
            };
            let n: i64 = value.parse().ok()?;
            let n = if *op == BinOp::Add { n } else { -n };
            let (dd, ss) = match unit.as_str() {
                "second" | "seconds" => (0, n),
                "minute" | "minutes" => (0, n * 60),
                "hour" | "hours" => (0, n * 3600),
                "day" | "days" => (n, 0),
                "week" | "weeks" => (n * 7, 0),
                _ => return None,
            };
            Some((f, d + dd, s + ss))
        }
        _ => None,
    }
}

/// The thresholds of one expression of an entity's records, or why it needs the periodic sweep.
fn scan(m: &Model, ent: &Entity, e: &Expr, out: &mut Vec<Threshold>) -> Result<(), String> {
    if !reads_clock(e) {
        // the clock may still be read through a function, a formula or another record
        if deps::analyze(m, &ent.name, &[e], &[]).time {
            return Err(format!(
                "the clock read through something else, on `{}`",
                ent.name
            ));
        }
        return Ok(());
    }
    match &e.kind {
        ExprKind::Binary {
            op: BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge,
            lhs,
            rhs,
        } => {
            let other = if is_clock_call(lhs, "now") {
                rhs
            } else if is_clock_call(rhs, "now") {
                lhs
            } else {
                return Err(format!(
                    "a comparison of `{}` with the clock that is not a threshold",
                    ent.name
                ));
            };
            match term(m, ent, other) {
                Some((field, days, secs)) => {
                    out.push(Threshold {
                        entity: ent.name.clone(),
                        field,
                        days,
                        secs,
                    });
                    Ok(())
                }
                None => Err(format!(
                    "`now()` compared with something other than a stored DateTime of `{}`",
                    ent.name
                )),
            }
        }
        ExprKind::Binary {
            op: BinOp::And | BinOp::Or | BinOp::Implies,
            lhs,
            rhs,
        } => {
            scan(m, ent, lhs, out)?;
            scan(m, ent, rhs, out)
        }
        ExprKind::Unary { expr, .. } => scan(m, ent, expr, out),
        _ if format!("{:?}", e.kind).contains("Name(\"today\")") => Err("`today()`".into()),
        _ => Err(format!(
            "the clock read in a way that is not a threshold, on `{}`",
            ent.name
        )),
    }
}

/// How the model changes with time alone: never, through thresholds only, or periodically.
pub fn schedule(m: &Model) -> Schedule {
    let mut exprs: Vec<(&Entity, &Expr)> = Vec::new();
    for e in &m.entities {
        for f in m.all_fields(e) {
            if let Some(x) = &f.formula {
                exprs.push((e, x));
            }
        }
        for c in &e.checks {
            exprs.push((e, &c.cond));
        }
    }
    for r in &m.reactions {
        if let (Trigger::Becomes(cond), Some(e)) = (&r.trigger, m.entity(&r.entity)) {
            exprs.push((e, cond));
        }
    }
    let mut out = Vec::new();
    let mut any = false;
    for (e, x) in exprs {
        if !deps::analyze(m, &e.name, &[x], &[]).time {
            continue;
        }
        any = true;
        let before = out.len();
        if let Err(why) = scan(m, e, x, &mut out) {
            return Schedule::Periodic(why);
        }
        if out.len() == before {
            return Schedule::Periodic(format!(
                "the clock read in a way that is not a threshold, on `{}`",
                e.name
            ));
        }
    }
    if !any {
        return Schedule::Never;
    }
    out.sort();
    out.dedup();
    Schedule::At(out)
}
