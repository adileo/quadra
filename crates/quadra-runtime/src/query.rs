//! Queries over whole entities, answered by the source in SQL when it can.
//!
//! The interpreter stays the reference: a query is a list of conditions (conjuncts of a
//! `where`), an order and what is wanted (rows, a count, an aggregate). The source translates
//! what it can into parameterized SQL over its projection tables (`quadra-store`, `sqlq.rs`)
//! and tells which conditions it could not translate and which rows the interpreter must
//! decide itself; the interpreter evaluates those on the rows SQL returns. Values that do not
//! depend on the row (`self.region`, `today()`, a local, `me`) are computed by the interpreter
//! beforehand and bound as parameters: SQL text is never built from values.

use crate::value::{Id, V};
use quadra_ir::{Expr, ExprKind, FPart, Model};
use std::collections::HashMap;

/// How a condition refers to the row it is about.
#[derive(Clone, Debug, PartialEq)]
pub enum Binder {
    /// a lambda parameter: `o => o.state is Confirmed`
    Param(String),
    /// `self` and the entity's members by their bare name: a formula of the entity (the API's
    /// `where`, columns and order)
    This,
}

/// Members every record has besides its declared fields.
pub const IMPLICIT: &[&str] = &["id", "created_at", "created_by", "updated_at", "updated_by", "version", "warnings"];

/// Values of the subexpressions that do not depend on the row, by the expression's address
/// (the expressions are borrowed for the whole query).
#[derive(Default)]
pub struct Frees {
    vals: HashMap<usize, Result<V, String>>,
}

impl Frees {
    pub fn key(e: &Expr) -> usize {
        e as *const Expr as usize
    }

    pub fn get(&self, e: &Expr) -> Option<&Result<V, String>> {
        self.vals.get(&Self::key(e))
    }

    pub fn insert(&mut self, e: &Expr, v: Result<V, String>) {
        self.vals.insert(Self::key(e), v);
    }

    pub fn contains(&self, e: &Expr) -> bool {
        self.vals.contains_key(&Self::key(e))
    }
}

/// Is `n` a name of the row (or of a row bound inside the condition)?
fn row_name(model: &Model, entity: &str, binder: &Binder, bound: &[String], n: &str) -> bool {
    if bound.iter().any(|b| b == n) {
        return true;
    }
    match binder {
        Binder::Param(p) => p == n,
        Binder::This => {
            IMPLICIT.contains(&n)
                || model.entity(entity).map_or(false, |e| {
                    model.find_field(e, n).is_some() || e.actions.iter().any(|a| a.name == n) || e.methods.iter().any(|m| m.name == n) || e.flows.iter().any(|f| f.transitions.iter().any(|t| t.by.as_deref() == Some(n)))
                })
        }
    }
}

/// Does `e` depend on the row (its binder, or a lambda parameter declared inside it)?
pub fn mentions_row(model: &Model, entity: &str, binder: &Binder, bound: &mut Vec<String>, e: &Expr) -> bool {
    let m = |x: &Expr, bound: &mut Vec<String>| mentions_row(model, entity, binder, bound, x);
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Error => false,
        ExprKind::Name(n) => row_name(model, entity, binder, bound, n),
        ExprKind::SelfRef => *binder == Binder::This,
        ExprKind::FString(parts) => parts.iter().any(|p| matches!(p, FPart::Expr(x, _) if m(x, bound))),
        ExprKind::Field { base, .. } => m(base, bound),
        ExprKind::Call { callee, args } => {
            // a method of the row called by its bare name
            if let ExprKind::Name(n) = &callee.kind {
                if row_name(model, entity, binder, bound, n) {
                    return true;
                }
            }
            m(callee, bound) || args.iter().any(|a| m(&a.value, bound))
        }
        ExprKind::Index { base, index } => m(base, bound) || m(index, bound),
        ExprKind::Unary { expr, .. } => m(expr, bound),
        ExprKind::Binary { lhs, rhs, .. } => m(lhs, bound) || m(rhs, bound),
        ExprKind::If { cond, then, otherwise } => m(cond, bound) || m(then, bound) || m(otherwise, bound),
        ExprKind::Lambda { params, body } => {
            let n = bound.len();
            bound.extend(params.iter().cloned());
            let r = m(body, bound);
            bound.truncate(n);
            r
        }
        ExprKind::Tuple(items) => items.iter().any(|(_, x)| m(x, bound)),
        ExprKind::List(args) => args.iter().any(|a| m(&a.value, bound)),
        ExprKind::Set(items) => items.iter().any(|x| m(x, bound)),
        ExprKind::When(arms) => arms.iter().any(|(c, v)| c.as_ref().map_or(false, |c| m(c, bound)) || m(v, bound)),
        ExprKind::Match { subject, arms } => m(subject, bound) || arms.iter().any(|a| m(&a.body, bound)),
        // statements may declare and assign anything: never computed apart
        ExprKind::Block(_) => true,
        ExprKind::As { body, .. } => m(body, bound),
        ExprKind::Fail(x) => m(x, bound),
    }
}

/// The largest subexpressions of `e` that do not depend on the row (nor on a lambda parameter
/// declared inside `e`): the interpreter computes them once, the source binds them as
/// parameters. Lambdas and blocks are never computed apart.
pub fn free_exprs<'e>(model: &Model, entity: &str, binder: &Binder, e: &'e Expr, out: &mut Vec<&'e Expr>) {
    fn walk<'e>(model: &Model, entity: &str, binder: &Binder, bound: &mut Vec<String>, e: &'e Expr, out: &mut Vec<&'e Expr>) {
        let free = !matches!(e.kind, ExprKind::Lambda { .. } | ExprKind::Block(_) | ExprKind::Fail(_) | ExprKind::Error) && !mentions_row(model, entity, binder, bound, e);
        if free {
            out.push(e);
            return;
        }
        let mut w = |x: &'e Expr, bound: &mut Vec<String>| walk(model, entity, binder, bound, x, out);
        match &e.kind {
            ExprKind::FString(parts) => {
                for p in parts {
                    if let FPart::Expr(x, _) = p {
                        w(x, bound);
                    }
                }
            }
            ExprKind::Field { base, .. } => w(base, bound),
            ExprKind::Call { callee, args } => {
                // `x.method(…)`: the receiver; a bare callee name is not a value
                if let ExprKind::Field { base, .. } = &callee.kind {
                    w(base, bound);
                }
                for a in args {
                    w(&a.value, bound);
                }
            }
            ExprKind::Index { base, index } => {
                w(base, bound);
                w(index, bound);
            }
            ExprKind::Unary { expr, .. } => w(expr, bound),
            ExprKind::Binary { lhs, rhs, .. } => {
                w(lhs, bound);
                w(rhs, bound);
            }
            ExprKind::If { cond, then, otherwise } => {
                w(cond, bound);
                w(then, bound);
                w(otherwise, bound);
            }
            ExprKind::Lambda { params, body } => {
                let n = bound.len();
                bound.extend(params.iter().cloned());
                w(body, bound);
                bound.truncate(n);
            }
            ExprKind::Tuple(items) => {
                for (_, x) in items {
                    w(x, bound);
                }
            }
            ExprKind::List(args) => {
                for a in args {
                    w(&a.value, bound);
                }
            }
            ExprKind::Set(items) => {
                for x in items {
                    w(x, bound);
                }
            }
            _ => {}
        }
    }
    walk(model, entity, binder, &mut Vec::new(), e, out)
}

/// The conjuncts of a condition: `a and (b and c)` → a, b, c.
pub fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match &e.kind {
        ExprKind::Binary { op: quadra_ir::BinOp::And, lhs, rhs } => {
            let mut v = conjuncts(lhs);
            v.extend(conjuncts(rhs));
            v
        }
        _ => vec![e],
    }
}

/// One condition of a query, with how it names the row.
#[derive(Clone, Copy)]
pub struct Cond<'a> {
    pub binder: &'a Binder,
    pub expr: &'a Expr,
    /// the rows where `expr` is not true (`all(p)`: is there a row where `p` is not true?)
    pub negate: bool,
}

/// An ordering key.
#[derive(Clone)]
pub enum Key<'a> {
    /// a value of the language (`sort_by`, `last_by`, `min`, `max`): compared as the language
    /// compares (no none, one currency or unit)
    Lang { binder: &'a Binder, expr: &'a Expr, desc: bool },
    /// a field path of the grid (`customer.name`), ordered as the grid shows it: numbers by
    /// value, text by its ASCII-lowercase form, none last (first when descending)
    Grid { path: Vec<String>, desc: bool },
}

/// Where a page of rows starts: after the row with these key values and id.
#[derive(Clone, Debug, PartialEq)]
pub struct After {
    pub keys: Vec<Option<String>>,
    pub id: Id,
}

/// An aggregate over the rows of a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFn {
    Sum,
    Min,
    Max,
    /// rows whose value is not none
    Count,
}

/// What an aggregate is computed on.
#[derive(Clone)]
pub enum AggOf<'a> {
    /// a value of the language (`sum(o => o.total)`)
    Expr { binder: &'a Binder, expr: &'a Expr },
    /// a field path of the grid (none is skipped)
    Path(Vec<String>),
}

#[derive(Clone)]
pub struct Agg<'a> {
    pub of: AggOf<'a>,
    pub func: AggFn,
}

/// How a date (or the date of a date-time) is grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Day,
    /// the Monday starting the week (ISO weeks)
    Week,
    Month,
    Quarter,
    Year,
}

impl Bucket {
    pub fn parse(s: &str) -> Option<Bucket> {
        Some(match s {
            "day" => Bucket::Day,
            "week" => Bucket::Week,
            "month" => Bucket::Month,
            "quarter" => Bucket::Quarter,
            "year" => Bucket::Year,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Bucket::Day => "day",
            Bucket::Week => "week",
            Bucket::Month => "month",
            Bucket::Quarter => "quarter",
            Bucket::Year => "year",
        }
    }

    /// The key of a date: `2026-03-05` (day), `2026-03-02` (the week's Monday), `2026-03`,
    /// `2026-Q1`, `2026`. In the order of the dates, compared as text.
    pub fn key(self, d: chrono::NaiveDate) -> String {
        use chrono::Datelike;
        match self {
            Bucket::Day => d.format("%Y-%m-%d").to_string(),
            Bucket::Week => (d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)).format("%Y-%m-%d").to_string(),
            Bucket::Month => d.format("%Y-%m").to_string(),
            Bucket::Quarter => format!("{}-Q{}", d.format("%Y"), (d.month() + 2) / 3),
            Bucket::Year => d.format("%Y").to_string(),
        }
    }

    /// The dates of a key: `[from, to)`.
    pub fn range(self, key: &str) -> Option<(chrono::NaiveDate, chrono::NaiveDate)> {
        use chrono::NaiveDate;
        let ym = |y: i32, m: u32| NaiveDate::from_ymd_opt(y, m, 1);
        let next_month = |y: i32, m: u32, n: u32| {
            let t = (m - 1 + n) as i32;
            ym(y + t / 12, (t % 12) as u32 + 1)
        };
        Some(match self {
            Bucket::Day => {
                let d = NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()?;
                (d, d.succ_opt()?)
            }
            Bucket::Week => {
                let d = NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()?;
                if Bucket::Week.key(d) != key {
                    return None;
                }
                (d, d + chrono::Duration::days(7))
            }
            Bucket::Month => {
                let (y, m) = key.split_once('-')?;
                let (y, m): (i32, u32) = (y.parse().ok()?, m.parse().ok()?);
                if key.len() != 7 || !(1..=12).contains(&m) {
                    return None;
                }
                (ym(y, m)?, next_month(y, m, 1)?)
            }
            Bucket::Quarter => {
                let (y, q) = key.split_once("-Q")?;
                let (y, q): (i32, u32) = (y.parse().ok()?, q.parse().ok()?);
                if y.to_string().len() != 4 || !(1..=4).contains(&q) {
                    return None;
                }
                (ym(y, q * 3 - 2)?, next_month(y, q * 3 - 2, 3)?)
            }
            Bucket::Year => {
                let y: i32 = key.parse().ok()?;
                if key.len() != 4 {
                    return None;
                }
                (ym(y, 1)?, ym(y + 1, 1)?)
            }
        })
    }
}

/// A key rows are grouped by: a field path (`customer.region`, `date`), a date bucketed.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupKey {
    pub path: Vec<String>,
    pub bucket: Option<Bucket>,
    /// the path goes through records the user may not all read (their values are hidden): the
    /// interpreter decides the rows of a group, not SQL
    pub hidden_path: bool,
}

/// How groups are ordered: by their keys, by their row count, or by an aggregate (its number:
/// the amount of money, the quantity; none, and several currencies or units, last).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GroupOrder {
    Key,
    Count,
    Agg(usize),
    /// by an aggregate (`by`; None: the row count), then by another one ascending (`then`: the
    /// first row of each group, `MIN(id)`): the order of the language's stable `sort_by` over
    /// groups in the order of their first row (`Entity.group_by(…).sort_by(…).first(n)`)
    Ranked { by: Option<usize>, then: usize },
}

/// Full-text search: every word is the beginning of a word of one of `fields` (stored text
/// the user may see). Words are runs of letters and digits, in lower case (`words`).
#[derive(Clone, Debug, PartialEq)]
pub struct TextSearch {
    pub words: Vec<String>,
    pub fields: Vec<String>,
}

/// A type holding text (`Text`, `Code`, `Email`…), optional or not.
pub fn text_type(t: Option<&quadra_ir::Ty>) -> bool {
    use quadra_ir::Ty;
    let t = match t {
        Some(Ty::Optional(t)) => Some(t.as_ref()),
        t => t,
    };
    matches!(t, Some(Ty::Path { segments, args, .. }) if args.is_empty() && matches!(segments.last().map(String::as_str), Some("Text" | "Code" | "Email" | "Phone" | "VatNumber" | "Barcode")))
}

/// The fields a full-text search looks in: the stored text fields of the entity (its own and
/// those of its traits; not formulas, not `per company`), in their order.
pub fn searchable(model: &Model, ent: &str) -> Vec<String> {
    let Some(e) = model.entity(ent) else {
        return Vec::new();
    };
    model.all_fields(e).iter().filter(|f| f.formula.is_none() && !f.per_company && !f.name.starts_with("__") && text_type(f.ty.as_ref())).map(|f| f.name.clone()).collect()
}

/// The words of a text, as the search indexes them: runs of letters and digits, in lower case.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// The words of a search (at most 10, each at most 64 characters).
pub fn search_words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words(text) {
        let w: String = w.chars().take(64).collect();
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out.truncate(10);
    out
}

/// Whether every word starts a word of one of the texts.
pub fn text_matches(search: &[String], texts: &[String]) -> bool {
    let have: Vec<String> = texts.iter().flat_map(|t| words(t)).collect();
    search.iter().all(|w| have.iter().any(|h| h.starts_with(w.as_str())))
}

/// The key of a value of a group, as text (None: no value): a record by its id, text and
/// variants as they are, integers, `true`/`false`, dates by their bucket.
pub fn group_value(v: &V, bucket: Option<Bucket>) -> Result<Option<String>, String> {
    Ok(Some(match v {
        V::None => return Ok(None),
        V::Ref { id, .. } => id.to_string(),
        V::Text(t) => t.to_string(),
        V::Variant { name, fields } if fields.is_empty() => name.to_string(),
        V::Int(i) => i.to_string(),
        V::Bool(b) => b.to_string(),
        V::Date(d) => bucket.unwrap_or(Bucket::Day).key(*d),
        V::DateTime(t) => bucket.unwrap_or(Bucket::Day).key(t.date()),
        other => return Err(format!("cannot group by a value like {other}")),
    }))
}

/// What a query asks for.
#[derive(Clone)]
pub enum What<'a> {
    /// rows in order (with the values of their keys), at most `limit` of them; `only_checked`:
    /// just the rows the interpreter must decide
    Rows { after: Option<After>, limit: usize, only_checked: bool },
    /// how many rows (counting at most `cap` + 1 of them), and how many the interpreter must decide
    Count { cap: Option<u64> },
    /// aggregates of the rows (the source answers only when no row needs the interpreter)
    Aggs(Vec<Agg<'a>>),
    /// the rows grouped by 1–2 keys: per group its key values, row count and aggregates, a page
    /// of groups in `order` (then the keys); `within`: per key, the values the groups may have
    /// (the cells of a pivot); the number of groups and the rows the interpreter must decide
    Groups { keys: Vec<GroupKey>, aggs: Vec<Agg<'a>>, order: GroupOrder, desc: bool, offset: usize, limit: usize, within: Vec<Option<Vec<Option<String>>>> },
}

/// The condition of a row rule (`where o => …`): its parameter and body.
#[derive(Clone, Copy, Debug)]
pub struct RuleCond<'a> {
    pub param: &'a str,
    pub body: &'a Expr,
}

/// The row rules that decide one verb on the rows of an entity for one user (phase 2: row
/// permissions in SQL), as `Interp::check_permission` applies them: refused when a `deny`
/// holds; otherwise allowed when the model has no role (`open`) or an `allow` holds; otherwise
/// a line follows its document (`parent`). A rule without a condition is `None` (it always
/// holds). An error while evaluating a condition the interpreter reaches refuses the row:
/// the source flags the rows where a condition may fail and the interpreter decides them.
#[derive(Clone, Debug)]
pub struct Perm<'a> {
    pub entity: String,
    pub deny: Vec<Option<RuleCond<'a>>>,
    pub allow: Vec<Option<RuleCond<'a>>>,
    pub open: bool,
    /// the `parent` field and the permission the document needs
    pub parent: Option<(String, Box<Perm<'a>>)>,
    /// the chain of documents is too long to follow in SQL
    pub too_deep: bool,
}

impl<'a> Perm<'a> {
    /// Every rule condition of the permission, with the entity whose rows it is about.
    pub fn conds(&self) -> Vec<(&str, RuleCond<'a>)> {
        let mut out: Vec<(&str, RuleCond<'a>)> = self.deny.iter().chain(self.allow.iter()).flatten().map(|c| (self.entity.as_str(), *c)).collect();
        if let Some((_, p)) = &self.parent {
            out.extend(p.conds());
        }
        out
    }
}

pub struct Request<'a> {
    pub model: &'a Model,
    pub entity: &'a str,
    pub conds: Vec<Cond<'a>>,
    pub order: Vec<Key<'a>>,
    /// ties (and the order without keys) by id descending
    pub id_desc: bool,
    pub frees: &'a Frees,
    /// records of the entity written in the working set: their committed rows are left out
    /// (the interpreter evaluates them itself)
    pub exclude: Vec<Id>,
    /// other entities written in the working set: what reads them is not translated
    pub dirty: Vec<String>,
    /// ids of the rows to keep (the selection of the grid)
    pub ids: Option<Vec<Id>>,
    /// rows whose reference (or set of references) holds this record: the lines of a record
    pub refs: Vec<(String, Id)>,
    /// keep only the rows the user may read (the source applies the rules in SQL when it can)
    pub perm: Option<&'a Perm<'a>>,
    pub what: What<'a>,
    /// the incremental formulas' columns hold the values the working set sees (nothing was
    /// written in it): they may be read like stored fields
    pub formulas: bool,
    /// rows whose key (`group_value`) is this value (None: no value): a group's rows
    pub keys: Vec<(GroupKey, Option<String>)>,
    /// rows matching a full-text search
    pub search: Option<TextSearch>,
}

/// Rows of a search: (id, cursor, the interpreter decides whether the user may read it), and
/// whether the row rules were applied in SQL.
pub struct Searched {
    pub rows: Vec<(Id, After, bool)>,
    pub perm_sql: bool,
}

pub struct Row {
    pub id: Id,
    /// the interpreter decides whether it matches
    pub check: bool,
    pub keys: Vec<Option<String>>,
}

/// The value of an aggregate as the database computed it: (value as text, its currency or
/// unit when it has one, how many distinct currencies or units, rows with a value).
#[derive(Clone, Debug, Default)]
pub struct AggOut {
    /// `int`, `num`, `money`, `qty`, `date`, `datetime`, `text`, `duration`
    pub kind: &'static str,
    pub value: Option<String>,
    pub unit: Option<String>,
    pub units: u64,
    pub rows: u64,
}

/// A group of rows as the database computed it: its key values (`group_value`), its rows,
/// those the interpreter must decide, its aggregates.
#[derive(Clone, Debug, Default)]
pub struct GroupOut {
    pub keys: Vec<Option<String>>,
    pub count: u64,
    pub flagged: u64,
    pub aggs: Vec<AggOut>,
}

#[derive(Default)]
pub struct Answer {
    pub rows: Vec<Row>,
    /// (rows, rows the interpreter must decide) for `What::Count`
    pub count: Option<(u64, u64)>,
    /// beyond the cap of a count: an estimate of the rows, and whether it is exact after all
    pub estimate: Option<(u64, bool)>,
    pub aggs: Vec<AggOut>,
    /// conditions (by position) the source did not translate: the interpreter evaluates them
    /// on every row returned
    pub residual: Vec<usize>,
    /// some rows may need the interpreter (a comparison with a value that may be none, another
    /// currency or unit…)
    pub guarded: bool,
    /// the request's `perm` was applied in SQL: the rows not flagged are readable; otherwise
    /// (`perm_engine` says why) SQL returned a superset and every row needs the interpreter
    pub perm_sql: bool,
    pub perm_engine: Option<String>,
    /// the entities and (entity, field) pairs the SQL read: the transaction relies on them
    pub entities: Vec<String>,
    pub fields: Vec<(String, String)>,
    /// `What::Groups`: the page of groups, and (groups, rows the interpreter must decide)
    pub groups: Vec<GroupOut>,
    pub groups_total: Option<(u64, u64)>,
}
