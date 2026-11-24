//! Typed Quadra expressions → parameterized SQL over the projection tables, on SQLite and
//! PostgreSQL.
//!
//! The translator follows the interpreter's semantics, not SQL's: a condition becomes `T`, a SQL
//! condition true exactly when the interpreter says true, and `G` (the guard), true on the rows
//! the translation cannot decide because the interpreter would do something SQL does not
//! (fail on a comparison with none, convert a quantity to another unit, compare two
//! currencies…). Those rows are returned flagged and the interpreter decides them. `not x` is
//! `NOT COALESCE(T(x), FALSE)`: SQL's third truth value never leaks out. What cannot be
//! translated at all (a formula, a user function, a method SQL has no equivalent for) is
//! reported per condition: the interpreter evaluates it on the rows SQL returns.
//!
//! Values that do not depend on the row arrive computed (`Frees`) and are bound as
//! parameters; SQL text is made only of identifiers from the schema and fixed keywords.
//!
//! Paths through references are joins (an inner join for a required reference: the column is
//! `NOT NULL` with a foreign key, so the join never drops a row; a left join otherwise);
//! collections (`lines`, `via`, sets) are `EXISTS` / `COUNT` subqueries correlated to their
//! owner. Phase 2 reuses `Tr` for row permissions: a rule is a condition on the row like any
//! other.

use crate::conn::{Dialect, Sql};
use crate::schema::{ColType, Part, Schema, Source as TS};
use quadra_ir::{Arg, BinOp, EntityKind, Expr, ExprKind, Model, Ty, UnOp};
use quadra_runtime::query::{After, AggFn, AggOf, Answer, Binder, Bucket, Frees, GroupKey, GroupOrder, Key, Perm, Request, RuleCond, TextSearch, What, IMPLICIT};
use quadra_runtime::V;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::collections::{BTreeSet, HashMap};

/// Queries answered in SQL by this process (tests check that a query did go to SQL).
pub static SELECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
thread_local! {
    static PERMS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// Queries of this thread whose row permissions were applied in SQL, and those left to the
/// interpreter row by row (tests check that SQL applied them).
pub fn perms() -> (u64, u64) {
    PERMS.with(|p| p.get())
}

fn perm_counted(sql: bool) {
    PERMS.with(|p| {
        let (a, b) = p.get();
        p.set(if sql { (a + 1, b) } else { (a, b + 1) });
    });
}

/// Why something is not translated.
pub type Untr = String;
type T<X> = Result<X, Untr>;

fn no<X>(why: impl Into<String>) -> T<X> {
    Err(why.into())
}

// ---- SQL fragments with their parameters -------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frag {
    pub sql: String,
    pub params: Vec<Sql>,
}

pub trait AddTo {
    fn add_to(self, f: &mut Frag);
}

impl AddTo for &str {
    fn add_to(self, f: &mut Frag) {
        f.sql.push_str(self);
    }
}

impl AddTo for String {
    fn add_to(self, f: &mut Frag) {
        f.sql.push_str(&self);
    }
}

impl AddTo for &String {
    fn add_to(self, f: &mut Frag) {
        f.sql.push_str(self);
    }
}

impl AddTo for &Frag {
    fn add_to(self, f: &mut Frag) {
        f.sql.push_str(&self.sql);
        f.params.extend(self.params.iter().cloned());
    }
}

impl AddTo for Frag {
    fn add_to(self, f: &mut Frag) {
        f.sql.push_str(&self.sql);
        f.params.extend(self.params);
    }
}

macro_rules! sql {
    ($($x:expr),* $(,)?) => {{
        let mut __f = Frag::default();
        $( AddTo::add_to($x, &mut __f); )*
        __f
    }};
}

impl Frag {
    /// A typed placeholder (`?text`, `?bigint`…) and its value.
    pub fn p(kind: &str, v: Sql) -> Frag {
        Frag { sql: format!("?{kind}"), params: vec![v] }
    }
}

fn q(name: &str) -> String {
    format!("\"{name}\"")
}

// ---- conditions ---------------------------------------------------------------------------

/// A SQL condition, folded when it is a constant.
#[derive(Clone, Debug, PartialEq)]
pub enum B {
    True,
    False,
    S(Frag),
}

fn and(a: B, b: B) -> B {
    match (a, b) {
        (B::False, _) | (_, B::False) => B::False,
        (B::True, x) | (x, B::True) => x,
        (B::S(a), B::S(b)) => B::S(sql!("(", &a, " AND ", &b, ")")),
    }
}

fn or(a: B, b: B) -> B {
    match (a, b) {
        (B::True, _) | (_, B::True) => B::True,
        (B::False, x) | (x, B::False) => x,
        (B::S(a), B::S(b)) => B::S(sql!("(", &a, " OR ", &b, ")")),
    }
}

/// `true`/`false` of a SQL condition in the dialect.
fn lit_bool(d: Dialect, b: bool) -> &'static str {
    match (d, b) {
        (Dialect::Postgres, true) => "TRUE",
        (Dialect::Postgres, false) => "FALSE",
        (Dialect::Sqlite, true) => "1",
        (Dialect::Sqlite, false) => "0",
    }
}

fn not(d: Dialect, a: B) -> B {
    match a {
        B::True => B::False,
        B::False => B::True,
        B::S(a) => B::S(sql!("NOT COALESCE(", &a, ", ", lit_bool(d, false), ")")),
    }
}

fn frag(b: &B) -> Frag {
    match b {
        B::True => sql!("(1=1)"),
        B::False => sql!("(1=0)"),
        B::S(f) => f.clone(),
    }
}

/// A condition: `t` true when the interpreter says true; `g` true on the rows the
/// interpreter must decide.
#[derive(Clone, Debug)]
struct C {
    t: B,
    g: B,
}

fn sure(t: B) -> C {
    C { t, g: B::False }
}

// ---- values -------------------------------------------------------------------------------

/// The kind of a scalar column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    Int,
    Num,
    Text,
    Enum,
    Date,
    DateTime,
    Bool,
    Duration,
    YearMonth,
}

/// Where a referenced record is: its alias when joined, or the join to make (in the scope of
/// the record holding the reference, under a key naming the path).
#[derive(Clone, Debug)]
struct Loc {
    scope: usize,
    key: String,
    alias: Option<String>,
}

#[derive(Clone, Debug)]
enum Coll {
    /// child rows pointing to the owner: `child.back = owner`
    Inverse { child: String, back: String, back_col: String, owner: Frag },
    /// many-to-many inverse: child rows whose set `back` holds the owner (its join table)
    InverseSet { child: String, back: String, jt: String, owner: Frag },
    /// a stored set or list of references of the owner (its join table)
    Stored { target: String, owner_ent: String, jt: String, owner: Frag, list: bool },
    /// every record of an entity
    Table { ent: String },
}

#[derive(Clone, Debug)]
enum Val {
    Const(V),
    Col { sql: Frag, k: K, null: bool },
    Money { amt: Frag, cur: Frag, null: bool, fixed: bool },
    Qty { v: Frag, unit: Frag, null: bool, fixed: bool },
    Ref { ent: String, id: Frag, null: bool, at: Loc },
    Coll(Coll),
}

struct Scope {
    alias: String,
    entity: String,
    /// lambda parameters naming this scope's row
    names: Vec<String>,
    /// `self` and bare field names name this scope's row (the top scope of a formula)
    this: bool,
    joins: Vec<Frag>,
    paths: HashMap<String, String>,
}

/// One translation: the scopes (the query's row, the subqueries' rows), what it read.
pub struct Tr<'a> {
    m: &'a Model,
    s: &'a Schema,
    d: Dialect,
    frees: &'a Frees,
    /// entities written in the working set (their committed rows are stale): not read
    dirty: &'a [String],
    scopes: Vec<Scope>,
    n: usize,
    /// failures met while reading values (none in a path without `?.`), collected by the
    /// condition being translated
    pending_guards: Vec<B>,
    /// names bound to records of the query (a row rule's parameter: the row, or its document)
    binds: Vec<(String, Val)>,
    pub entities: BTreeSet<String>,
    pub fields: BTreeSet<(String, String)>,
    /// the columns of the incremental formulas may be read (`Request::formulas`)
    pub formulas: bool,
}

fn ty_entity(t: Option<&Ty>) -> Option<String> {
    match t? {
        Ty::Path { segments, .. } if segments.len() == 1 => Some(segments[0].clone()),
        Ty::Optional(i) => ty_entity(Some(i)),
        _ => None,
    }
}

fn coll_entity(t: Option<&Ty>) -> Option<(String, bool)> {
    match t? {
        Ty::List(i) => ty_entity(Some(i)).map(|e| (e, false)),
        Ty::Set(i) => ty_entity(Some(i)).map(|e| (e, true)),
        Ty::Optional(i) => coll_entity(Some(i)),
        _ => None,
    }
}

/// The decimal text of a number, as the projections store it.
fn dec_text(d: &Decimal) -> String {
    d.normalize().to_string()
}

impl<'a> Tr<'a> {
    pub fn new(m: &'a Model, s: &'a Schema, d: Dialect, frees: &'a Frees, dirty: &'a [String], root: &str) -> T<Tr<'a>> {
        let e = m.entity(root).ok_or_else(|| format!("unknown entity `{root}`"))?;
        if e.kind != EntityKind::Entity || e.is_ledger || e.temporary || s.entity_table(root).is_none() {
            return no(format!("`{root}` has no table of its own"));
        }
        let mut t = Tr { m, s, d, frees, dirty, scopes: Vec::new(), n: 0, pending_guards: Vec::new(), binds: Vec::new(), entities: BTreeSet::new(), fields: BTreeSet::new(), formulas: false };
        t.entities.insert(root.to_string());
        t.scopes.push(Scope { alias: "t0".into(), entity: root.to_string(), names: Vec::new(), this: false, joins: Vec::new(), paths: HashMap::new() });
        Ok(t)
    }

    fn alias(&mut self, p: &str) -> String {
        self.n += 1;
        format!("{p}{}", self.n)
    }

    fn table_of(&self, ent: &str) -> T<String> {
        self.s.entity_table(ent).map(|t| t.name.clone()).ok_or_else(|| format!("`{ent}` has no table"))
    }

    // ---- dialect helpers ----

    fn num_param(&self, d: &Decimal) -> Frag {
        match self.d {
            Dialect::Postgres => Frag::p("numeric", Sql::Text(dec_text(d))),
            Dialect::Sqlite => Frag::p("text", Sql::Text(dec_text(d))),
        }
    }

    /// A numeric column as SQL compares it exactly: on SQLite decimals are text, compared
    /// with the `quadra_dec` collation.
    fn num_col(&self, c: &Frag) -> Frag {
        match self.d {
            Dialect::Postgres => c.clone(),
            Dialect::Sqlite => sql!("(", c, " COLLATE quadra_dec)"),
        }
    }

    /// Text compared byte by byte (as Rust compares strings).
    fn text_col(&self, c: &Frag) -> Frag {
        match self.d {
            Dialect::Postgres => sql!("(", c, " COLLATE \"C\")"),
            Dialect::Sqlite => c.clone(),
        }
    }

    fn bool_param(&self, b: bool) -> Frag {
        Frag::p("boolean", Sql::Bool(b))
    }

    fn is_null(&self, f: &Frag) -> B {
        B::S(sql!("(", f, " IS NULL)"))
    }

    fn not_null(&self, f: &Frag) -> B {
        B::S(sql!("(", f, " IS NOT NULL)"))
    }

    /// The rows where the interpreter decides: only in the query's own scope (inside a
    /// subquery the interpreter would decide per child, which SQL cannot hand back).
    fn guard(&self, reach: &B, err: B) -> T<B> {
        if err == B::False {
            return Ok(B::False);
        }
        if self.scopes.len() > 1 {
            return no("a condition inside a collection that may fail");
        }
        Ok(and(reach.clone(), err))
    }

    // ---- names and paths ----

    fn row(&self, i: usize) -> Val {
        let s = &self.scopes[i];
        Val::Ref { ent: s.entity.clone(), id: sql!(format!("{}.\"id\"", s.alias)), null: false, at: Loc { scope: i, key: String::new(), alias: Some(s.alias.clone()) } }
    }

    fn free(&self, e: &Expr) -> Option<T<Val>> {
        self.frees.get(e).map(|r| match r {
            Ok(v) => Ok(Val::Const(v.clone())),
            Err(m) => Err(format!("a value failed: {m}")),
        })
    }

    fn is_field(&self, ent: &str, n: &str) -> bool {
        IMPLICIT.contains(&n) || self.m.entity(ent).map_or(false, |e| self.m.find_field(e, n).is_some())
    }

    fn name(&mut self, n: &str, reach: &B) -> T<Val> {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].names.iter().any(|x| x == n) {
                return Ok(self.row(i));
            }
            if self.scopes[i].this && self.is_field(&self.scopes[i].entity.clone(), n) {
                let r = self.row(i);
                return self.member(r, n, false, reach);
            }
            if i == 0 {
                if let Some((_, v)) = self.binds.iter().rev().find(|(b, _)| b == n) {
                    return Ok(v.clone());
                }
            }
        }
        no(format!("`{n}` is not known here"))
    }

    /// The alias of a referenced record, joining it when needed.
    fn alias_of(&mut self, ent: &str, id: &Frag, null: bool, at: &Loc) -> T<String> {
        if let Some(a) = &at.alias {
            return Ok(a.clone());
        }
        if let Some(a) = self.scopes[at.scope].paths.get(&at.key) {
            return Ok(a.clone());
        }
        let table = self.table_of(ent)?;
        let a = self.alias("j");
        let join = if null { "LEFT JOIN" } else { "JOIN" };
        let j = sql!(format!(" {join} {} {a} ON {a}.\"id\" = ", q(&table)), id);
        self.scopes[at.scope].joins.push(j);
        self.scopes[at.scope].paths.insert(at.key.clone(), a.clone());
        Ok(a)
    }

    fn scope_of_alias(&self, alias: &str) -> usize {
        for (i, s) in self.scopes.iter().enumerate().rev() {
            if s.alias == alias || s.paths.values().any(|a| a == alias) {
                return i;
            }
        }
        0
    }

    /// `rec.name` of a record value.
    fn member(&mut self, base: Val, name: &str, optional: bool, reach: &B) -> T<Val> {
        let (ent, id, rnull, at) = match base {
            Val::Ref { ent, id, null, at } => (ent, id, null, at),
            Val::Const(V::None) if optional => return Ok(Val::Const(V::None)),
            // the number of an amount or a quantity
            Val::Money { amt, null, .. } if name == "raw" => return Ok(Val::Col { sql: amt, k: K::Num, null }),
            Val::Qty { v, null, .. } if name == "raw" => return Ok(Val::Col { sql: v, k: K::Num, null }),
            Val::Col { sql, k: k @ (K::Num | K::Int), null } if name == "raw" => return Ok(Val::Col { sql, k, null }),
            _ => return no(format!("`.{name}` of something that is not a record")),
        };
        let e = self.m.entity(&ent).ok_or_else(|| format!("unknown entity `{ent}`"))?;
        if e.kind != EntityKind::Entity || e.is_ledger {
            return no(format!("`{ent}` is not an entity with a table"));
        }
        // the query's row is excluded when written here; any other record of a written entity
        // would be read as committed
        let own_row = at.alias.as_deref() == Some("t0");
        if !own_row && self.dirty.iter().any(|d| *d == ent) {
            return no(format!("`{ent}` has records written in this transaction"));
        }
        // the record's id (a group's first row)
        if name == "id" && self.m.find_field(e, name).is_none() {
            return Ok(Val::Col { sql: id, k: K::Int, null: rnull });
        }
        // who created the record: the `_created_by` column of its projection
        if name == "created_by" && self.m.find_field(e, name).is_none() {
            let t = self.s.entity_table(&ent).ok_or("no table")?;
            if !t.columns.iter().any(|c| c.name == "_created_by") {
                return no(format!("`{ent}.created_by` has no column"));
            }
            let mut err = B::False;
            if rnull && !optional {
                err = self.is_null(&id);
            }
            let g = self.guard(reach, err)?;
            if g != B::False {
                self.pending_guards.push(g);
            }
            self.entities.insert(ent.clone());
            let alias = self.alias_of(&ent, &id, rnull, &at)?;
            let scope = self.scope_of_alias(&alias);
            let key = format!("{alias}._created_by");
            return Ok(Val::Ref { ent: "User".into(), id: sql!(format!("{alias}.\"_created_by\"")), null: true, at: Loc { scope, key, alias: None } });
        }
        let Some(f) = self.m.find_field(e, name) else {
            return no(format!("`{ent}.{name}` (not a stored field)"));
        };
        let formula = f.formula.is_some();
        if formula {
            // an incremental formula: its column holds its value (phase 3)
            let t = self.s.entity_table(&ent).ok_or("no table")?;
            let fm = t.fields.iter().find(|m| m.field == name && m.formula);
            if !self.formulas || !fm.is_some_and(|m| m.incremental) {
                return no(format!("`{ent}.{name}` is a formula"));
            }
        }
        if f.per_company {
            return no(format!("`{ent}.{name}` is per company"));
        }
        // reading through none: the interpreter fails unless `?.`
        let mut err = B::False;
        if rnull && !optional {
            err = self.is_null(&id);
        }
        let g = self.guard(reach, err)?;
        if g != B::False {
            self.pending_guards.push(g);
        }
        self.entities.insert(ent.clone());
        // collections computed from the other side
        if let (false, Some((child, set))) = (formula, coll_entity(f.ty.as_ref())) {
            let owner_alias = self.alias_of(&ent, &id, rnull, &at)?;
            let owner = sql!(format!("{owner_alias}.\"id\""));
            let ce = self.m.entity(&child).ok_or_else(|| format!("unknown entity `{child}`"))?;
            if crate::schema::is_inverse(self.m, e, f) {
                if f.through.is_some() {
                    return no("a collection `through` another entity");
                }
                if ce.kind != EntityKind::Entity {
                    return no(format!("a collection of `{child}`"));
                }
                let back = f.via.clone().or_else(|| {
                    ce.fields
                        .iter()
                        .find(|x| {
                            let n = ty_entity(x.ty.as_ref());
                            n.as_deref() == Some(ent.as_str()) || n.map_or(false, |n| e.traits.contains(&n))
                        })
                        .map(|x| x.name.clone())
                });
                let Some(back) = back else {
                    return no("a collection without its back reference");
                };
                let bf = self.m.find_field(ce, &back).ok_or("no back field")?;
                if bf.formula.is_some() {
                    return no("a collection through a formula");
                }
                if matches!(bf.ty, Some(Ty::Set(_))) {
                    let jt = self.s.tables.iter().find(|t| matches!(&t.source, TS::Join(e2, f2) if *e2 == child && *f2 == back)).map(|t| t.name.clone()).ok_or("no join table")?;
                    return Ok(Val::Coll(Coll::InverseSet { child, back, jt, owner }));
                }
                let t = self.s.entity_table(&child).ok_or("no child table")?;
                let fm = t.fields.iter().find(|m| m.field == back && !m.formula && m.columns.len() == 1 && m.columns[0].part == Part::RefId).ok_or("the back reference has no column")?;
                return Ok(Val::Coll(Coll::Inverse { child, back, back_col: fm.columns[0].name.clone(), owner }));
            }
            // a stored set (or list) of references: its join table
            let jt = self.s.tables.iter().find(|t| matches!(&t.source, TS::Join(e2, f2) if *e2 == ent && *f2 == name)).map(|t| t.name.clone());
            let Some(jt) = jt else {
                return no(format!("`{ent}.{name}` is not a set of records"));
            };
            self.fields.insert((ent.clone(), name.to_string()));
            return Ok(Val::Coll(Coll::Stored { target: child, owner_ent: ent.clone(), jt, owner, list: !set }));
        }
        let t = self.s.entity_table(&ent).ok_or("no table")?;
        let Some(fm) = t.fields.iter().find(|m| m.field == name && m.formula == formula) else {
            return no(format!("`{ent}.{name}` has no column"));
        };
        let fm = fm.clone();
        let alias = self.alias_of(&ent, &id, rnull, &at)?;
        self.fields.insert((ent.clone(), name.to_string()));
        let col = |c: &str| sql!(format!("{alias}.{}", q(c)));
        if formula {
            // empty when the formula has no value or failed: the interpreter tells them apart
            let first = col(&fm.columns[0].name);
            let g = self.guard(reach, self.is_null(&first))?;
            if g != B::False {
                self.pending_guards.push(g);
            }
        }
        let cnull = |i: usize| rnull || !fm.columns[i].not_null;
        let fixed_unit = |i: usize| -> bool {
            // `Money<EUR>`, `Qty<pcs>`: a code, not a path from the record
            let ty = match &f.ty {
                Some(Ty::Optional(t)) => Some(t.as_ref()),
                t => t.as_ref(),
            };
            match ty {
                Some(Ty::Path { args, .. }) => match args.get(i) {
                    Some(Ty::Path { segments, .. }) => segments.len() == 1 && !self.is_field(&ent, &segments[0]) && segments[0] != "self" && segments[0] != "current",
                    _ => false,
                },
                _ => false,
            }
        };
        let parts: Vec<Part> = fm.columns.iter().map(|c| c.part).collect();
        Ok(match parts.as_slice() {
            [Part::RefId] => {
                let target = ty_entity(f.ty.as_ref()).ok_or("a reference without its entity")?;
                let key = format!("{alias}.{}", fm.columns[0].name);
                let scope = self.scope_of_alias(&alias);
                Val::Ref { ent: target, id: col(&fm.columns[0].name), null: cnull(0), at: Loc { scope, key, alias: None } }
            }
            [Part::Scalar] => {
                let c = &fm.columns[0];
                let ym = matches!(&f.ty, Some(Ty::Path { segments, .. }) if segments.last().map(String::as_str) == Some("YearMonth"))
                    || matches!(&f.ty, Some(Ty::Optional(t)) if matches!(t.as_ref(), Ty::Path { segments, .. } if segments.last().map(String::as_str) == Some("YearMonth")));
                let k = match c.ty {
                    ColType::BigInt => K::Int,
                    ColType::Numeric => K::Num,
                    ColType::Text if c.check.is_some() => K::Enum,
                    ColType::Text if ym => K::YearMonth,
                    ColType::Text => K::Text,
                    ColType::Bool => K::Bool,
                    ColType::Date => K::Date,
                    ColType::Timestamp => K::DateTime,
                    ColType::Json => return no("a value stored as JSON"),
                };
                // a role or a variant-typed text column: compared as text
                Val::Col { sql: col(&c.name), k, null: cnull(0) }
            }
            [Part::Duration] => Val::Col { sql: col(&fm.columns[0].name), k: K::Duration, null: cnull(0) },
            [Part::Amount, Part::Currency] => Val::Money { amt: col(&fm.columns[0].name), cur: col(&fm.columns[1].name), null: cnull(0), fixed: fixed_unit(0) },
            [Part::Amount, Part::Unit] => Val::Qty { v: col(&fm.columns[0].name), unit: col(&fm.columns[1].name), null: cnull(0), fixed: fixed_unit(0) },
            _ => return no(format!("`{ent}.{name}`: a value of several parts")),
        })
    }

    // ---- values ----

    fn val(&mut self, e: &Expr, reach: &B) -> T<Val> {
        if let Some(v) = self.free(e) {
            return v;
        }
        match &e.kind {
            ExprKind::Name(n) => self.name(n, reach),
            ExprKind::SelfRef => match self.scopes.iter().position(|s| s.this) {
                Some(i) => Ok(self.row(i)),
                None => no("`self`"),
            },
            ExprKind::Field { base, name, optional } => {
                let b = self.val(base, reach)?;
                self.member(b, name, *optional, reach)
            }
            ExprKind::Call { callee, args } => self.call_val(callee, args, reach),
            ExprKind::Binary { op: BinOp::Coalesce, lhs, rhs } => {
                let l = self.val(lhs, reach)?;
                let lnull = match &l {
                    Val::Col { sql, null: true, .. } => self.is_null(sql),
                    Val::Col { null: false, .. } => B::False,
                    Val::Ref { id, null: true, .. } => self.is_null(id),
                    Val::Ref { null: false, .. } => B::False,
                    Val::Const(V::None) => B::True,
                    Val::Const(_) => B::False,
                    _ => return no("`??` on this value"),
                };
                let r = self.val(rhs, &and(reach.clone(), lnull))?;
                match (l, r) {
                    (Val::Const(V::None), r) => Ok(r),
                    (l @ Val::Const(_), _) => Ok(l),
                    (Val::Col { sql: a, k, null: _ }, Val::Col { sql: b, k: k2, null: n2 }) if k == k2 => Ok(Val::Col { sql: sql!("COALESCE(", &a, ", ", &b, ")"), k, null: n2 }),
                    (Val::Col { sql: a, k, .. }, Val::Const(v)) => match self.scalar_param(k, &v)? {
                        Some(p) => Ok(Val::Col { sql: sql!("COALESCE(", &a, ", ", &p, ")"), k, null: false }),
                        None => Ok(Val::Col { sql: a, k, null: true }),
                    },
                    (Val::Ref { ent, id: a, at, .. }, Val::Ref { ent: e2, id: b, null: n2, .. }) if ent == e2 => {
                        // a new record: joined under its own key
                        let key = format!("coalesce({}|{})", a.sql, b.sql);
                        let at = Loc { scope: at.scope, key, alias: None };
                        Ok(Val::Ref { ent, id: sql!("COALESCE(", &a, ", ", &b, ")"), null: n2, at })
                    }
                    _ => no("`??` between these values"),
                }
            }
            _ => no("this expression"),
        }
    }

    /// A constant as a parameter of a column of kind `k` (None for none); an error when the
    /// interpreter would not compare them as SQL does.
    fn scalar_param(&self, k: K, v: &V) -> T<Option<Frag>> {
        Ok(Some(match (k, v) {
            (_, V::None) => return Ok(None),
            (K::Int, V::Int(i)) => Frag::p("bigint", Sql::Int(*i)),
            (K::Num, x @ (V::Int(_) | V::Dec(_) | V::Pct(_))) => self.num_param(&x.num().unwrap()),
            (K::Text, V::Text(t)) => Frag::p("text", Sql::Text(t.to_string())),
            (K::Enum, V::Variant { name, fields }) if fields.is_empty() => Frag::p("text", Sql::Text(name.to_string())),
            (K::Date, V::Date(d)) => Frag::p("date", Sql::Text(d.to_string())),
            (K::DateTime, V::DateTime(d)) => Frag::p("timestamp", Sql::Text(d.format("%Y-%m-%d %H:%M:%S").to_string())),
            (K::Bool, V::Bool(b)) => self.bool_param(*b),
            (K::Duration, V::Duration(s)) => Frag::p("bigint", Sql::Int(*s)),
            (K::YearMonth, V::YearMonth(y, m)) => Frag::p("text", Sql::Text(format!("{y}-{m:02}"))),
            _ => return no(format!("a {k:?} column and {v}")),
        }))
    }

    /// Methods whose value is not a condition: `len()`, `count(…)` of a collection.
    fn call_val(&mut self, callee: &Expr, args: &[Arg], reach: &B) -> T<Val> {
        let ExprKind::Field { base, name, optional } = &callee.kind else {
            return no("a call");
        };
        if name == "count" {
            let (coll, mut lambdas) = self.coll_chain(base, reach)?;
            if let Coll::Stored { list: true, .. } = coll {
                return no("the count of a stored list (its join table has no duplicates)");
            }
            lambdas.extend(self.lambdas(args, 1)?);
            let (sub, _) = self.subquery(&coll, &lambdas, false)?;
            return Ok(Val::Col { sql: sql!("(SELECT COUNT(*) ", &sub, ")"), k: K::Int, null: false });
        }
        let b = self.val(base, reach)?;
        match (&b, name.as_str()) {
            (Val::Col { sql: c, k: K::Text, null }, "len") if args.is_empty() => {
                let err = if *null && !*optional { self.is_null(c) } else { B::False };
                let g = self.guard(reach, err)?;
                if g != B::False {
                    self.pending_guards.push(g);
                }
                let f = match self.d {
                    Dialect::Sqlite => sql!("length(", c, ")"),
                    Dialect::Postgres => sql!("char_length(", c, ")"),
                };
                Ok(Val::Col { sql: f, k: K::Int, null: *null })
            }
            _ => no(format!("`.{name}(…)` here")),
        }
    }

    /// The lambdas of a call (`where` chains collect them): at most `max` positional arguments,
    /// each a literal lambda of one parameter.
    fn lambdas<'e>(&self, args: &'e [Arg], max: usize) -> T<Vec<(String, &'e Expr)>> {
        if args.len() > max || args.iter().any(|a| a.name.is_some() || a.spread) {
            return no("arguments");
        }
        let mut out = Vec::new();
        for a in args {
            match &a.value.kind {
                ExprKind::Lambda { params, body } if params.len() == 1 => out.push((params[0].clone(), body.as_ref())),
                _ => return no("an argument that is not a lambda"),
            }
        }
        Ok(out)
    }

    /// `FROM … WHERE …` of a collection's rows, with the conditions of its lambdas (one row
    /// scope named by every lambda's parameter; `negate_last`: the last lambda's condition is
    /// negated, for `all`). Returns the alias of the collection's row too.
    fn subquery(&mut self, coll: &Coll, lambdas: &[(String, &Expr)], negate_last: bool) -> T<(Frag, String)> {
        let (ent, from, alias, link) = match coll {
            Coll::Inverse { child, back, back_col, owner } => {
                let a = self.alias("s");
                self.fields.insert((child.clone(), back.clone()));
                (child.clone(), sql!(format!("FROM {} {a}", q(&self.table_of(child)?))), a.clone(), sql!(format!("{a}.{} = ", q(back_col)), owner))
            }
            Coll::InverseSet { child, back, jt, owner } => {
                let a = self.alias("s");
                let j = self.alias("k");
                self.fields.insert((child.clone(), back.clone()));
                (child.clone(), sql!(format!("FROM {} {j} JOIN {} {a} ON {a}.\"id\" = {j}.\"owner_id\"", q(jt), q(&self.table_of(child)?))), a.clone(), sql!(format!("{j}.\"target_id\" = "), owner))
            }
            Coll::Stored { target, owner_ent, jt, owner, .. } => {
                if self.dirty.iter().any(|d| d == owner_ent) {
                    return no(format!("`{owner_ent}` has records written in this transaction"));
                }
                let a = self.alias("s");
                let j = self.alias("k");
                (target.clone(), sql!(format!("FROM {} {j} JOIN {} {a} ON {a}.\"id\" = {j}.\"target_id\"", q(jt), q(&self.table_of(target)?))), a.clone(), sql!(format!("{j}.\"owner_id\" = "), owner))
            }
            Coll::Table { ent } => {
                let a = self.alias("s");
                (ent.clone(), sql!(format!("FROM {} {a}", q(&self.table_of(ent)?))), a.clone(), sql!("1=1"))
            }
        };
        let ce = self.m.entity(&ent).ok_or("unknown entity")?;
        if ce.kind != EntityKind::Entity || ce.is_ledger {
            return no(format!("a collection of `{ent}`"));
        }
        if self.dirty.iter().any(|d| *d == ent) {
            return no(format!("`{ent}` has records written in this transaction"));
        }
        self.entities.insert(ent.clone());
        self.scopes.push(Scope { alias: alias.clone(), entity: ent, names: lambdas.iter().map(|(p, _)| p.clone()).collect(), this: false, joins: Vec::new(), paths: HashMap::new() });
        let saved = std::mem::take(&mut self.pending_guards);
        let mut conds = B::True;
        let mut r = Ok(());
        for (i, (_, body)) in lambdas.iter().enumerate() {
            match self.cond(body, &B::True) {
                Ok(c) if c.g != B::False => {
                    r = no("a condition inside a collection that may fail");
                    break;
                }
                Ok(c) => {
                    let t = if negate_last && i + 1 == lambdas.len() { not(self.d, c.t) } else { c.t };
                    conds = and(conds, t);
                }
                Err(e) => {
                    r = Err(e);
                    break;
                }
            }
        }
        let inner = std::mem::replace(&mut self.pending_guards, saved);
        let sc = self.scopes.pop().unwrap();
        r?;
        if !inner.is_empty() {
            return no("a value inside a collection that may fail");
        }
        let mut f = from;
        for j in &sc.joins {
            f = sql!(f, j);
        }
        Ok((sql!(f, " WHERE ", &link, " AND ", frag(&conds)), alias))
    }

    /// A collection and the conditions of the `where`s applied to it: `lines.where(l => …)`.
    fn coll_chain<'e>(&mut self, e: &'e Expr, reach: &B) -> T<(Coll, Vec<(String, &'e Expr)>)> {
        if let ExprKind::Call { callee, args } = &e.kind {
            if let ExprKind::Field { base, name, .. } = &callee.kind {
                if name == "where" && self.free(e).is_none() {
                    let (c, mut l) = self.coll_chain(base, reach)?;
                    let more = self.lambdas(args, 1)?;
                    if more.is_empty() {
                        return no("`where` without a condition");
                    }
                    l.extend(more);
                    return Ok((c, l));
                }
            }
        }
        match self.val(e, reach)? {
            Val::Coll(c) => Ok((c, Vec::new())),
            Val::Const(V::Table(t)) => Ok((Coll::Table { ent: t.to_string() }, Vec::new())),
            _ => no("a collection"),
        }
    }

    // ---- conditions ----

    fn cond(&mut self, e: &Expr, reach: &B) -> T<C> {
        if let Some(v) = self.free(e) {
            let v = v?;
            let Val::Const(v) = v else { unreachable!() };
            return Ok(sure(if v.truthy() { B::True } else { B::False }));
        }
        match &e.kind {
            ExprKind::Binary { op: BinOp::And, lhs, rhs } => {
                let a = self.cond(lhs, reach)?;
                // `false and …`: the right side is never evaluated (nor joined)
                if a.t == B::False && a.g == B::False {
                    return Ok(a);
                }
                let b = self.cond(rhs, &and(reach.clone(), a.t.clone()))?;
                Ok(C { t: and(a.t, b.t), g: or(a.g, b.g) })
            }
            ExprKind::Binary { op: BinOp::Or, lhs, rhs } => {
                let a = self.cond(lhs, reach)?;
                // `true or …` (a filter not set: `__p.count() == 0 or …`): the right side is
                // never evaluated (nor joined)
                if a.t == B::True && a.g == B::False {
                    return Ok(a);
                }
                let b = self.cond(rhs, &and(reach.clone(), not(self.d, a.t.clone())))?;
                Ok(C { t: or(a.t, b.t), g: or(a.g, b.g) })
            }
            ExprKind::Binary { op: BinOp::Implies, lhs, rhs } => {
                let a = self.cond(lhs, reach)?;
                let b = self.cond(rhs, &and(reach.clone(), a.t.clone()))?;
                Ok(C { t: or(not(self.d, a.t), b.t), g: or(a.g, b.g) })
            }
            ExprKind::Unary { op: UnOp::Not, expr } => {
                let a = self.cond(expr, reach)?;
                Ok(C { t: not(self.d, a.t), g: a.g })
            }
            ExprKind::If { cond, then, otherwise } => {
                let c = self.cond(cond, reach)?;
                let a = self.cond(then, &and(reach.clone(), c.t.clone()))?;
                let nc = not(self.d, c.t.clone());
                let b = self.cond(otherwise, &and(reach.clone(), nc.clone()))?;
                Ok(C { t: or(and(c.t, a.t), and(nc, b.t)), g: or(c.g, or(a.g, b.g)) })
            }
            ExprKind::Binary { op, lhs, rhs } if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::In | BinOp::NotIn | BinOp::Is | BinOp::IsNot) => {
                let saved = std::mem::take(&mut self.pending_guards);
                let lr = self.val(lhs, reach).and_then(|l| self.val(rhs, reach).map(|r| (l, r)));
                let pg = std::mem::replace(&mut self.pending_guards, saved);
                let (l, r) = lr?;
                let mut c = match op {
                    BinOp::Eq => self.eq(&l, &r)?,
                    BinOp::Ne => {
                        let c = self.eq(&l, &r)?;
                        C { t: not(self.d, c.t), g: c.g }
                    }
                    BinOp::In | BinOp::NotIn => {
                        let c = self.member_of(&l, &r)?;
                        if *op == BinOp::In {
                            c
                        } else {
                            C { t: not(self.d, c.t), g: c.g }
                        }
                    }
                    BinOp::Is | BinOp::IsNot => {
                        let c = self.is(&l, &r)?;
                        if *op == BinOp::Is {
                            c
                        } else {
                            C { t: not(self.d, c.t), g: c.g }
                        }
                    }
                    _ => self.ord(*op, &l, &r)?,
                };
                // the comparison's own failures happen where it is evaluated
                c.g = self.guard(reach, c.g)?;
                for g in pg {
                    c.g = or(c.g, g);
                }
                Ok(c)
            }
            ExprKind::Call { callee, args } => self.call_cond(callee, args, reach),
            ExprKind::Field { .. } | ExprKind::Name(_) | ExprKind::SelfRef | ExprKind::Binary { op: BinOp::Coalesce, .. } => {
                let saved = std::mem::take(&mut self.pending_guards);
                let v = self.val(e, reach);
                let pg = std::mem::replace(&mut self.pending_guards, saved);
                let v = v?;
                let mut c = self.truthy(&v)?;
                for g in pg {
                    c.g = or(c.g, g);
                }
                Ok(c)
            }
            _ => no("this condition"),
        }
    }

    fn truthy(&self, v: &Val) -> T<C> {
        match v {
            Val::Const(v) => Ok(sure(if v.truthy() { B::True } else { B::False })),
            Val::Col { sql: c, k: K::Bool, .. } => Ok(sure(B::S(sql!("(", c, " = ", self.bool_param(true), ")")))),
            _ => no("a value used as a condition"),
        }
    }

    /// Methods that are conditions: `starts_with`, `contains`, `any`, `all`, `none`…
    fn call_cond(&mut self, callee: &Expr, args: &[Arg], reach: &B) -> T<C> {
        // what reading the receiver and arguments may fail on is collected here
        let saved = std::mem::take(&mut self.pending_guards);
        let r = self.call_cond_inner(callee, args, reach);
        let pg = std::mem::replace(&mut self.pending_guards, saved);
        let mut c = r?;
        for g in pg {
            c.g = or(c.g, g);
        }
        Ok(c)
    }

    fn call_cond_inner(&mut self, callee: &Expr, args: &[Arg], reach: &B) -> T<C> {
        let ExprKind::Field { base, name, optional } = &callee.kind else {
            return no("a call");
        };
        if matches!(name.as_str(), "any" | "none" | "all") {
            let (coll, mut lambdas) = self.coll_chain(base, reach)?;
            let own = self.lambdas(args, 1)?;
            if name == "all" && own.is_empty() {
                return Ok(sure(B::True));
            }
            let negate = name == "all";
            lambdas.extend(own);
            // `all(p)`: no row (after the `where`s) where `p` is not true
            let (sub, _) = self.subquery(&coll, &lambdas, negate)?;
            let ex = sql!("EXISTS (SELECT 1 ", &sub, ")");
            return Ok(sure(if name == "any" { B::S(ex) } else { B::S(sql!("NOT ", ex)) }));
        }
        if name == "contains" {
            // a collection, or else text
            let saved = self.pending_guards.len();
            match self.coll_chain(base, reach) {
                Ok((coll, lambdas)) => {
                    if args.len() != 1 || args[0].name.is_some() {
                        return no("arguments");
                    }
                    let x = self.val(&args[0].value, reach)?;
                    return self.contains(&coll, &lambdas, &x);
                }
                Err(_) => self.pending_guards.truncate(saved),
            }
        }
        let b = self.val(base, reach)?;
        let c = match (&b, name.as_str()) {
            (Val::Col { sql: c, k: K::Text, null }, "starts_with" | "ends_with" | "contains") => {
                if args.len() != 1 || args[0].name.is_some() {
                    return no("arguments");
                }
                let Some(Ok(arg)) = self.frees.get(&args[0].value) else {
                    return no("an argument that depends on the row");
                };
                // the interpreter compares with the argument's text
                let s = arg.to_string();
                let p = Frag::p("text", Sql::Text(s.clone()));
                let t = match (name.as_str(), self.d) {
                    ("starts_with", Dialect::Sqlite) => {
                        sql!("(substr(", c, ", 1, length(", &p, ")) = ", &p, ")")
                    }
                    ("starts_with", Dialect::Postgres) => {
                        sql!("(left(", c, ", char_length(", &p, ")) = ", &p, ")")
                    }
                    ("ends_with", _) if s.is_empty() => sql!("(", c, " IS NOT NULL)"),
                    ("ends_with", Dialect::Sqlite) => {
                        sql!("(substr(", c, ", -length(", &p, ")) = ", &p, ")")
                    }
                    ("ends_with", Dialect::Postgres) => {
                        sql!("(right(", c, ", char_length(", &p, ")) = ", &p, ")")
                    }
                    (_, Dialect::Sqlite) => sql!("(instr(", c, ", ", &p, ") > 0)"),
                    (_, Dialect::Postgres) => sql!("(strpos(", c, ", ", &p, ") > 0)"),
                };
                let err = if *null && !*optional { self.is_null(c) } else { B::False };
                C { t: B::S(t), g: self.guard(reach, err)? }
            }
            _ => return no(format!("`.{name}(…)` here")),
        };
        Ok(c)
    }

    /// Whether a collection holds a value (`==` on its elements: references by id).
    fn contains(&mut self, coll: &Coll, lambdas: &[(String, &Expr)], x: &Val) -> T<C> {
        let id = match x {
            Val::Const(V::Ref { id, .. }) => Frag::p("bigint", Sql::Int(*id as i64)),
            Val::Const(_) => return Ok(sure(B::False)),
            // a NULL id finds nothing: none is in no collection of records
            Val::Ref { id, .. } => id.clone(),
            _ => return no("`contains` of this value"),
        };
        let t = match coll {
            Coll::Stored { jt, owner, owner_ent, .. } if lambdas.is_empty() => {
                if self.dirty.iter().any(|d| d == owner_ent) {
                    return no(format!("`{owner_ent}` has records written in this transaction"));
                }
                let k = self.alias("k");
                sql!(format!("EXISTS (SELECT 1 FROM {} {k} WHERE {k}.\"owner_id\" = ", q(jt)), owner, format!(" AND {k}.\"target_id\" = "), &id, ")")
            }
            _ => {
                let (sub, a) = self.subquery(coll, lambdas, false)?;
                sql!("EXISTS (SELECT 1 ", &sub, format!(" AND {a}.\"id\" = "), &id, ")")
            }
        };
        Ok(sure(B::S(t)))
    }

    // ---- comparisons ----

    /// `l == r` as the interpreter's `==`.
    fn eq(&mut self, l: &Val, r: &Val) -> T<C> {
        let (l, r) = match (l, r) {
            (Val::Const(_), x) if !matches!(x, Val::Const(_)) => (r, l),
            _ => (l, r),
        };
        Ok(match (l, r) {
            (Val::Const(a), Val::Const(b)) => {
                if matches!((a, b), (V::Qty { .. }, V::Qty { .. })) {
                    return no("two quantities");
                }
                sure(if a == b { B::True } else { B::False })
            }
            (Val::Col { sql: c, .. }, Val::Const(V::None)) => sure(self.is_null(c)),
            (Val::Money { amt, .. }, Val::Const(V::None)) => sure(self.is_null(amt)),
            (Val::Qty { v, .. }, Val::Const(V::None)) => sure(self.is_null(v)),
            (Val::Ref { id, .. }, Val::Const(V::None)) => sure(self.is_null(id)),
            (Val::Col { sql: c, k, .. }, Val::Const(v)) => match (k, v) {
                (K::Int, V::Dec(d) | V::Pct(d)) if d.fract().is_zero() => sure(B::S(sql!("(", c, " = ", Frag::p("bigint", Sql::Int(d.to_i64().ok_or("a large number")?)), ")"))),
                (K::Int, V::Dec(_) | V::Pct(_)) => sure(B::False),
                (K::Num, x @ (V::Int(_) | V::Dec(_) | V::Pct(_))) => sure(B::S(sql!("(", self.num_col(c), " = ", self.num_param(&x.num().unwrap()), ")"))),
                (K::Duration, x) if x.num().is_some_and(|n| n.is_zero()) => sure(B::S(sql!("(", c, " = 0)"))),
                (K::Date, V::DateTime(_)) | (K::DateTime, V::Date(_)) | (K::Enum, V::Text(_)) | (K::Text, V::Variant { .. }) => sure(B::False),
                _ => match self.scalar_param(*k, v)? {
                    Some(p) => sure(B::S(sql!("(", c, " = ", &p, ")"))),
                    None => sure(self.is_null(c)),
                },
            },
            (Val::Money { amt, cur, null, .. }, Val::Const(v)) => {
                let bare = and(self.is_null(cur), if *null { self.not_null(amt) } else { B::True });
                match v {
                    V::Money { amt: a, cur: c } => C { t: B::S(sql!("(", self.num_col(amt), " = ", self.num_param(a), " AND ", cur, " = ", Frag::p("text", Sql::Text(c.to_string())), ")")), g: bare },
                    V::Int(0) => C { t: B::S(sql!("(", self.num_col(amt), " = ", self.num_param(&Decimal::ZERO), ")")), g: B::False },
                    _ => return no("an amount and this value"),
                }
            }
            (Val::Qty { v, unit, null, .. }, Val::Const(x)) => match x {
                V::Qty { v: b, unit: w } => {
                    let wp = Frag::p("text", Sql::Text(w.to_string()));
                    let other = B::S(sql!("(", unit, " IS NULL OR ", unit, " <> ", &wp, ")"));
                    C { t: B::S(sql!("(", unit, " = ", &wp, " AND ", self.num_col(v), " = ", self.num_param(b), ")")), g: and(if *null { self.not_null(v) } else { B::True }, other) }
                }
                V::Int(0) => C { t: B::S(sql!("(", self.num_col(v), " = ", self.num_param(&Decimal::ZERO), ")")), g: B::False },
                _ => return no("a quantity and this value"),
            },
            (Val::Ref { id, .. }, Val::Const(V::Ref { id: x, .. })) => sure(B::S(sql!("(", id, " = ", Frag::p("bigint", Sql::Int(*x as i64)), ")"))),
            (Val::Ref { .. }, Val::Const(_)) => sure(B::False),
            (Val::Ref { id: a, null: na, .. }, Val::Ref { id: b, null: nb, .. }) => {
                let both = if *na && *nb { and(self.is_null(a), self.is_null(b)) } else { B::False };
                sure(or(B::S(sql!("(", a, " = ", b, ")")), both))
            }
            (Val::Col { sql: a, k: ka, null: na }, Val::Col { sql: b, k: kb, null: nb }) if ka == kb => {
                let (a2, b2) = if *ka == K::Num { (self.num_col(a), b.clone()) } else { (a.clone(), b.clone()) };
                let both = if *na && *nb { and(self.is_null(a), self.is_null(b)) } else { B::False };
                sure(or(B::S(sql!("(", &a2, " = ", &b2, ")")), both))
            }
            _ => return no("`==` between these values"),
        })
    }

    /// `<`, `<=`, `>`, `>=` as the interpreter's `compare` (which fails on none, on two
    /// currencies, on units it cannot convert).
    fn ord(&mut self, op: BinOp, l: &Val, r: &Val) -> T<C> {
        let flip = |op: BinOp| match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::Le => BinOp::Ge,
            BinOp::Gt => BinOp::Lt,
            BinOp::Ge => BinOp::Le,
            o => o,
        };
        let (op, l, r) = match (l, r) {
            (Val::Const(_), x) if !matches!(x, Val::Const(_)) => (flip(op), r, l),
            _ => (op, l, r),
        };
        let sym = match op {
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            _ => ">=",
        };
        let cmp = |a: Frag, b: Frag| B::S(sql!("(", a, format!(" {sym} "), b, ")"));
        Ok(match (l, r) {
            (Val::Const(_), Val::Const(_)) => return no("two constants"),
            (Val::Col { sql: c, k, null }, Val::Const(v)) => {
                let nullg = if *null { self.is_null(c) } else { B::False };
                if v.is_none() {
                    return Ok(C { t: B::False, g: B::True });
                }
                let t = match (k, v) {
                    (K::Int, V::Int(i)) => cmp(c.clone(), Frag::p("bigint", Sql::Int(*i))),
                    (K::Int, V::Dec(d) | V::Pct(d)) => {
                        // an integer against a fraction: compare with the integer on the right side of it
                        let bound = match op {
                            BinOp::Lt | BinOp::Ge => d.ceil(),
                            _ => d.floor(),
                        };
                        cmp(c.clone(), Frag::p("bigint", Sql::Int(bound.to_i64().ok_or("a large number")?)))
                    }
                    (K::Num, x @ (V::Int(_) | V::Dec(_) | V::Pct(_))) => cmp(self.num_col(c), self.num_param(&x.num().unwrap())),
                    (K::Text, V::Text(t)) => cmp(self.text_col(c), Frag::p("text", Sql::Text(t.to_string()))),
                    (K::Date, V::Date(d)) => cmp(c.clone(), Frag::p("date", Sql::Text(d.to_string()))),
                    // a date against a date-time: its day
                    (K::Date, V::DateTime(dt)) => cmp(c.clone(), Frag::p("date", Sql::Text(dt.date().to_string()))),
                    (K::DateTime, V::DateTime(dt)) => cmp(c.clone(), Frag::p("timestamp", Sql::Text(dt.format("%Y-%m-%d %H:%M:%S").to_string()))),
                    (K::Duration, V::Duration(s)) => cmp(c.clone(), Frag::p("bigint", Sql::Int(*s))),
                    (K::Duration, x) if x.num().is_some_and(|n| n.is_zero()) => cmp(c.clone(), sql!("0")),
                    _ => return no(format!("`{sym}` between a {k:?} column and {v}")),
                };
                C { t, g: nullg }
            }
            (Val::Money { amt, cur, null, .. }, Val::Const(v)) => {
                let nullg = if *null { self.is_null(amt) } else { B::False };
                match v {
                    V::Money { amt: a, cur: c } => {
                        let other = B::S(sql!("(", cur, " IS NULL OR ", cur, " <> ", Frag::p("text", Sql::Text(c.to_string())), ")"));
                        C { t: cmp(self.num_col(amt), self.num_param(a)), g: or(nullg, other) }
                    }
                    x if x.num().is_some() => C { t: cmp(self.num_col(amt), self.num_param(&x.num().unwrap())), g: nullg },
                    _ => return no("an amount and this value"),
                }
            }
            (Val::Qty { v, unit, null, .. }, Val::Const(x)) => {
                let nullg = if *null { self.is_null(v) } else { B::False };
                match x {
                    V::Qty { v: b, unit: w } => {
                        let wp = Frag::p("text", Sql::Text(w.to_string()));
                        let other = B::S(sql!("(", unit, " IS NULL OR ", unit, " <> ", &wp, ")"));
                        C { t: cmp(self.num_col(v), self.num_param(b)), g: or(nullg, other) }
                    }
                    x if x.num().is_some() => C { t: cmp(self.num_col(v), self.num_param(&x.num().unwrap())), g: nullg },
                    _ => return no("a quantity and this value"),
                }
            }
            (Val::Col { sql: a, k: ka, null: na }, Val::Col { sql: b, k: kb, null: nb }) if ka == kb && matches!(ka, K::Int | K::Num | K::Text | K::Date | K::DateTime | K::Duration) => {
                let (a2, b2) = match ka {
                    K::Num => (self.num_col(a), b.clone()),
                    K::Text => (self.text_col(a), b.clone()),
                    _ => (a.clone(), b.clone()),
                };
                let g = or(if *na { self.is_null(a) } else { B::False }, if *nb { self.is_null(b) } else { B::False });
                C { t: cmp(a2, b2), g }
            }
            (Val::Money { amt: a, cur: ca, null: na, .. }, Val::Money { amt: b, cur: cb, null: nb, .. }) => {
                let g = or(or(if *na { self.is_null(a) } else { B::False }, if *nb { self.is_null(b) } else { B::False }), B::S(sql!("(", ca, " IS NULL OR ", cb, " IS NULL OR ", ca, " <> ", cb, ")")));
                C { t: cmp(self.num_col(a), b.clone()), g }
            }
            (Val::Qty { v: a, unit: ua, null: na, .. }, Val::Qty { v: b, unit: ub, null: nb, .. }) => {
                let g = or(or(if *na { self.is_null(a) } else { B::False }, if *nb { self.is_null(b) } else { B::False }), B::S(sql!("(", ua, " IS NULL OR ", ub, " IS NULL OR ", ua, " <> ", ub, ")")));
                C { t: cmp(self.num_col(a), b.clone()), g }
            }
            _ => return no(format!("`{sym}` between these values")),
        })
    }

    /// `x in xs` (`==` with an element of a list or set, or within a date range).
    fn member_of(&mut self, l: &Val, r: &Val) -> T<C> {
        match r {
            Val::Const(V::List(items) | V::Set(items)) => {
                if items.len() > 500 {
                    return no("a list of more than 500 values");
                }
                let mut t = B::False;
                let mut g = B::False;
                for it in items.iter() {
                    let c = self.eq(l, &Val::Const(it.clone()))?;
                    t = or(t, c.t);
                    g = or(g, c.g);
                }
                Ok(C { t, g })
            }
            Val::Const(V::DateRange(a, b)) => match l {
                Val::Col { sql: c, k: K::Date, .. } => Ok(sure(B::S(sql!("(", c, " >= ", Frag::p("date", Sql::Text(a.to_string())), " AND ", c, " <= ", Frag::p("date", Sql::Text(b.to_string())), ")")))),
                Val::Col { sql: c, k: K::DateTime, .. } => {
                    let end = b.succ_opt().ok_or("a date out of range")?;
                    Ok(sure(B::S(sql!("(", c, " >= ", Frag::p("timestamp", Sql::Text(format!("{a} 00:00:00"))), " AND ", c, " < ", Frag::p("timestamp", Sql::Text(format!("{end} 00:00:00"))), ")"))))
                }
                Val::Const(_) => no("two constants"),
                _ => Ok(sure(B::False)),
            },
            Val::Const(_) => Ok(sure(B::False)),
            Val::Coll(coll) => {
                let coll = coll.clone();
                self.contains(&coll, &[], l)
            }
            _ => no("`in` this value"),
        }
    }

    /// `x is State`, `x is none`.
    fn is(&mut self, l: &Val, r: &Val) -> T<C> {
        match (l, r) {
            (Val::Col { sql: c, k: K::Enum, .. }, Val::Const(V::Variant { name, .. })) => Ok(sure(B::S(sql!("(", c, " = ", Frag::p("text", Sql::Text(name.to_string())), ")")))),
            (_, Val::Const(V::None)) => self.eq(l, r),
            (Val::Ref { ent, .. }, Val::Const(V::Table(t))) => {
                let hit = **t == **ent || self.m.entity(ent).map_or(false, |e| self.m.implements(e, t));
                if self.m.entity(ent).map_or(true, |e| e.kind == EntityKind::Trait) {
                    return no("`is` on a trait value");
                }
                Ok(sure(if hit { B::True } else { B::False }))
            }
            (Val::Col { k, .. }, Val::Const(V::Variant { .. })) if *k != K::Enum => Ok(sure(B::False)),
            (Val::Const(V::None), _) => self.eq(r, l),
            _ => no("`is` between these values"),
        }
    }
}

// ---- row permissions ---------------------------------------------------------------------------

/// The ids of rows of one entity, as SQL: what a permission lets through (or flags).
#[derive(Clone, Debug, PartialEq)]
enum Set {
    Empty,
    /// every row
    All,
    /// every row but these (a compound `SELECT` of ids)
    AllBut(Frag),
    /// these rows (a compound `SELECT` of ids)
    Some(Frag),
}

/// Whether a `SELECT` is compound at its top level (`… UNION …`): as the right operand of
/// another compound it must be wrapped (compound operators associate to the left).
fn compound(f: &Frag) -> bool {
    let mut depth = 0i32;
    let b = f.sql.as_bytes();
    for (i, c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b' ' if depth == 0 => {
                let rest = &f.sql[i..];
                if rest.starts_with(" UNION ") || rest.starts_with(" EXCEPT ") || rest.starts_with(" INTERSECT ") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// `a op b`, `b` wrapped when compound.
fn setop(a: Frag, op: &str, b: Frag) -> Frag {
    if compound(&b) {
        sql!(a, format!(" {op} SELECT * FROM ("), b, ") qw")
    } else {
        sql!(a, format!(" {op} "), b)
    }
}

fn union(a: Set, b: Set) -> Set {
    match (a, b) {
        (Set::All, _) | (_, Set::All) => Set::All,
        (Set::Empty, x) | (x, Set::Empty) => x,
        (Set::Some(a), Set::Some(b)) => Set::Some(setop(a, "UNION", b)),
        // every row but `d`, or `s`: every row but those of `d` not in `s`
        (Set::AllBut(d), Set::Some(s)) | (Set::Some(s), Set::AllBut(d)) => Set::AllBut(setop(d, "EXCEPT", s)),
        (Set::AllBut(a), Set::AllBut(b)) => Set::AllBut(setop(a, "INTERSECT", b)),
    }
}

/// `a` without the rows of `d`.
fn minus(a: Set, d: Set) -> Set {
    match (a, d) {
        (_, Set::All) | (Set::Empty, _) => Set::Empty,
        (a, Set::Empty) => a,
        (Set::All, Set::Some(d)) => Set::AllBut(d),
        (Set::Some(s), Set::Some(d)) => Set::Some(setop(s, "EXCEPT", d)),
        (Set::AllBut(x), Set::Some(d)) => Set::AllBut(setop(x, "UNION", d)),
        // without every row but `x`: what is in `x`
        (Set::All, Set::AllBut(x)) => Set::Some(x),
        (Set::Some(s), Set::AllBut(x)) => Set::Some(setop(s, "INTERSECT", x)),
        (Set::AllBut(y), Set::AllBut(x)) => Set::Some(setop(x, "EXCEPT", y)),
    }
}

/// The disjuncts of a condition: `a or (b or c)` → a, b, c.
fn disjuncts(e: &Expr) -> Vec<&Expr> {
    match &e.kind {
        ExprKind::Binary { op: BinOp::Or, lhs, rhs } => {
            let mut v = disjuncts(lhs);
            v.extend(disjuncts(rhs));
            v
        }
        _ => vec![e],
    }
}

/// The pieces of a permission on one entity: the rows denied, the rows its own rules allow,
/// the document's readable rows, and the rows each may flag for the interpreter.
struct PermParts {
    deny: Set,
    allow: Set,
    guard: Set,
    /// the `parent` column and the document's (readable, flagged) rows
    parent: Option<(String, Set, Set)>,
}

impl<'a> Tr<'a> {
    /// A translation of a condition about rows of `ent` inside this one (a subquery): its own
    /// row alias, aliases that do not clash with this query's.
    fn sub(&mut self, ent: &str) -> T<Tr<'a>> {
        let mut t = Tr::new(self.m, self.s, self.d, self.frees, self.dirty, ent)?;
        t.formulas = self.formulas;
        t.n = self.n;
        let a = t.alias("p");
        t.scopes[0].alias = a;
        Ok(t)
    }

    fn absorb(&mut self, t: Tr<'a>) {
        self.n = t.n;
        self.entities.extend(t.entities);
        self.fields.extend(t.fields);
    }

    /// The rows of `ent` where one disjunct of a rule's condition holds, and those where the
    /// interpreter must decide (its guard): each a `SELECT` of ids over the entity and the
    /// joins of its paths, so that the database can start from the user (`salesperson = ?`)
    /// rather than from every row.
    fn disjunct_sets(&mut self, ent: &str, param: &str, e: &Expr) -> T<(Set, Set)> {
        let mut sub = self.sub(ent)?;
        sub.bind_top(&Binder::Param(param.to_string()));
        sub.pending_guards.clear();
        let c = sub.cond(e, &B::True)?;
        if !sub.pending_guards.is_empty() {
            return no("a value that may fail outside a comparison");
        }
        let alias = sub.scopes[0].alias.clone();
        let table = sub.table_of(ent)?;
        let mut from = sql!(format!("SELECT {alias}.\"id\" FROM {} {alias}", q(&table)));
        for j in &sub.scopes[0].joins {
            from = sql!(from, j);
        }
        // (the joins of paths never drop a row: true is every row)
        let set = |b: &B| match b {
            B::False => Set::Empty,
            B::True => Set::All,
            b => Set::Some(sql!(&from, " WHERE ", frag(b))),
        };
        let r = (set(&c.t), set(&c.g));
        self.absorb(sub);
        Ok(r)
    }

    /// The rows of `ent` where a rule (all of it) holds, and those it flags.
    fn rule_sets(&mut self, ent: &str, rc: &Option<RuleCond>) -> T<(Set, Set)> {
        let Some(rc) = rc else {
            return Ok((Set::All, Set::Empty));
        };
        let mut t = Set::Empty;
        let mut g = Set::Empty;
        for d in disjuncts(rc.body) {
            let (a, b) = self.disjunct_sets(ent, rc.param, d)?;
            t = union(t, a);
            g = union(g, b);
        }
        Ok((t, g))
    }

    fn perm_parts(&mut self, p: &Perm) -> T<PermParts> {
        if p.too_deep {
            return no("documents nested too deep");
        }
        let mut deny = Set::Empty;
        let mut guard = Set::Empty;
        for d in &p.deny {
            let (t, g) = self.rule_sets(&p.entity, d)?;
            deny = union(deny, t);
            guard = union(guard, g);
        }
        if p.open {
            return Ok(PermParts { deny, allow: Set::All, guard, parent: None });
        }
        let mut allow = Set::Empty;
        for a in &p.allow {
            let (t, g) = self.rule_sets(&p.entity, a)?;
            allow = union(allow, t);
            guard = union(guard, g);
        }
        let mut parent = None;
        if let (Some((field, pp)), false) = (&p.parent, allow == Set::All) {
            let t = self.s.entity_table(&p.entity).ok_or("no table")?;
            let col = t.fields.iter().find(|m| m.field == *field && !m.formula && m.columns.len() == 1 && m.columns[0].part == Part::RefId).ok_or("the parent reference has no column")?.columns[0].name.clone();
            self.fields.insert((p.entity.clone(), field.clone()));
            let (pset, pguard) = self.perm_set(pp)?;
            parent = Some((col, pset, pguard));
        }
        Ok(PermParts { deny, allow, guard, parent })
    }

    /// The readable rows of a permission's entity, and the rows the interpreter must decide.
    fn perm_set(&mut self, p: &Perm) -> T<(Set, Set)> {
        let parts = self.perm_parts(p)?;
        let mut allow = parts.allow;
        let mut guard = parts.guard;
        if let Some((col, pset, pguard)) = parts.parent {
            let table = self.table_of(&p.entity)?;
            let x = self.alias("p");
            let children = |s: &Set| -> Set {
                let base = sql!(format!("SELECT {x}.\"id\" FROM {} {x} WHERE ", q(&table)));
                let c = sql!(format!("{x}.{}", q(&col)));
                match s {
                    Set::Empty => Set::Empty,
                    Set::All => Set::Some(sql!(base, &c, " IS NOT NULL")),
                    Set::AllBut(d) => Set::Some(sql!(base, &c, " IS NOT NULL AND ", &c, " NOT IN (", d, ")")),
                    Set::Some(s) => Set::Some(sql!(base, &c, " IN (", s, ")")),
                }
            };
            allow = union(allow, children(&pset));
            guard = union(guard, children(&pguard));
        }
        let readable = minus(allow, parts.deny);
        if readable == Set::Empty {
            // refused whatever the conditions (the interpreter would refuse too)
            return Ok((Set::Empty, Set::Empty));
        }
        Ok((readable, guard))
    }

    /// `id` (a column holding ids of the set's entity, maybe NULL) is in `s`.
    fn in_set(&self, s: &Set, id: &Frag, null: bool) -> B {
        match s {
            Set::Empty => B::False,
            Set::All if !null => B::True,
            Set::All => B::S(sql!("(", id, " IS NOT NULL)")),
            Set::AllBut(d) if !null => B::S(sql!("(", id, " NOT IN (", d, "))")),
            Set::AllBut(d) => B::S(sql!("(", id, " IS NOT NULL AND ", id, " NOT IN (", d, "))")),
            Set::Some(s) => B::S(sql!("(", id, " IN (", s, "))")),
        }
    }

    /// A permission as a condition on the query's row (`t0`): `T` the rows readable, `G` the
    /// rows the interpreter decides. Its own rules and the deny rules are sets of ids, the
    /// document's permission a set of ids its `parent` column must be in.
    fn perm_cond(&mut self, p: &Perm) -> T<C> {
        if self.d == Dialect::Postgres {
            // PostgreSQL plans conditions joined by `or` across the joined tables well (from
            // the user's rows); SQLite does not, and gets the sets of ids
            let row = self.row(0);
            return self.perm_inline(p, row);
        }
        let parts = self.perm_parts(p)?;
        let id = sql!("t0.\"id\"");
        let mut allow = self.in_set(&parts.allow, &id, false);
        let mut g = self.in_set(&parts.guard, &id, false);
        if let Some((col, pset, pguard)) = &parts.parent {
            let c = sql!(format!("t0.{}", q(col)));
            allow = or(allow, self.in_set(pset, &c, true));
            g = or(g, self.in_set(pguard, &c, true));
        }
        let deny = self.in_set(&parts.deny, &id, false);
        let t = and(not(self.d, deny), allow);
        if t == B::False {
            return Ok(sure(B::False));
        }
        Ok(C { t, g })
    }
}

impl<'a> Tr<'a> {
    /// A rule's condition on a record of the query (`row`: the query's row or its document,
    /// joined): its paths are joins of the query.
    fn rule_inline(&mut self, rc: &RuleCond, row: &Val) -> T<C> {
        let saved = self.binds.len();
        self.scopes[0].names.clear();
        self.scopes[0].this = false;
        self.binds.push((rc.param.to_string(), row.clone()));
        self.pending_guards.clear();
        let r = self.cond(rc.body, &B::True);
        self.binds.truncate(saved);
        let c = r?;
        if !self.pending_guards.is_empty() {
            return no("a value that may fail outside a comparison");
        }
        Ok(c)
    }

    /// A permission as a condition on `row`, its rules and its document's joined to the query.
    fn perm_inline(&mut self, p: &Perm, row: Val) -> T<C> {
        let mut deny = B::False;
        let mut g = B::False;
        for d in &p.deny {
            match d {
                None => deny = B::True,
                Some(rc) => {
                    let c = self.rule_inline(rc, &row)?;
                    deny = or(deny, c.t);
                    g = or(g, c.g);
                }
            }
        }
        let mut allow = if p.open { B::True } else { B::False };
        if !p.open {
            for a in &p.allow {
                match a {
                    None => allow = B::True,
                    Some(rc) => {
                        let c = self.rule_inline(rc, &row)?;
                        allow = or(allow, c.t);
                        g = or(g, c.g);
                    }
                }
            }
            if allow != B::True {
                if p.too_deep {
                    return no("documents nested too deep");
                }
                if let Some((field, pp)) = &p.parent {
                    let pv = self.member(row.clone(), field, true, &B::True)?;
                    let Val::Ref { id, null, .. } = &pv else {
                        return no("a parent that is not a record");
                    };
                    let there = if *null { self.not_null(id) } else { B::True };
                    let pc = self.perm_inline(pp, pv.clone())?;
                    allow = or(allow, and(there, pc.t));
                    g = or(g, pc.g);
                }
            }
        }
        if deny == B::True || allow == B::False {
            return Ok(sure(B::False));
        }
        Ok(C { t: and(not(self.d, deny), allow), g })
    }
}

/// Whether a rule's condition runs in the database: per rule of the model with a condition
/// and per entity it applies to, `Ok` (SQL) or why the interpreter evaluates it row by row.
/// The interpreter computes the values that do not depend on the row (`me` as a user with
/// id 0 when it has none).
#[derive(Clone, Debug)]
pub struct RuleSql {
    /// the role, None at the top level
    pub role: Option<String>,
    /// `read, edit Customer where …`
    pub rule: String,
    pub entity: String,
    pub span: quadra_ir::Span,
    pub sql: Result<(), String>,
}

/// The verdict of every rule with a condition (see `RuleSql`), translated as a query would.
pub fn rules_sql(i: &mut quadra_runtime::Interp, s: &Schema, d: Dialect) -> Vec<RuleSql> {
    let m = i.m;
    let saved = i.user;
    if i.user.is_none() {
        i.user = Some(0);
    }
    let mut out = Vec::new();
    let all: Vec<(Option<String>, &quadra_ir::Rule)> = m.roles.iter().flat_map(|r| r.rules.iter().map(move |x| (Some(r.name.clone()), x))).chain(m.rules.iter().map(|x| (None, x))).collect();
    for (role, rule) in all {
        let Some(ExprKind::Lambda { params, body }) = rule.condition.as_ref().map(|c| &c.kind) else {
            continue;
        };
        if !matches!(rule.effect.as_str(), "allow" | "deny") {
            continue;
        }
        let rc = RuleCond { param: params.first().map(String::as_str).unwrap_or("_"), body: body.as_ref() };
        let mut ents: Vec<String> = Vec::new();
        for t in &rule.targets {
            match t.as_slice() {
                [x] if x == "*" => ents.extend(m.entities.iter().filter(|e| e.kind == EntityKind::Entity && !e.temporary && !e.is_ledger).map(|e| e.name.clone())),
                [e, ..] => ents.push(e.clone()),
                _ => {}
            }
        }
        ents.dedup();
        let text = format!(
            "{}{} {} where {}",
            if rule.effect == "deny" { "deny " } else { "" },
            rule.verbs.join(", "),
            rule.targets.iter().map(|t| t.join(".")).collect::<Vec<_>>().join(", "),
            quadra_ir::print::expr(rule.condition.as_ref().unwrap())
        );
        for ent in ents {
            let perm = Perm { entity: ent.clone(), deny: Vec::new(), allow: vec![Some(rc)], open: false, parent: None, too_deep: false };
            let mut frees = Frees::default();
            i.perm_frees(&perm, &mut frees);
            let r = Tr::new(m, s, d, &frees, &[], &ent).and_then(|mut tr| tr.rule_sets(&ent, &Some(rc)).map(|_| ()));
            out.push(RuleSql { role: role.clone(), rule: text.clone(), entity: ent, span: rule.span.clone(), sql: r });
        }
    }
    i.user = saved;
    out
}

/// Whether a condition on the rows of `ent` translates to SQL (the editor's hints): Ok(true)
/// when SQL decides every row, Ok(false) when it returns some rows flagged for the interpreter
/// (a value that may be none, of another currency or unit), Err with the reason otherwise.
pub fn cond_sql(d: Dialect, s: &Schema, m: &Model, ent: &str, binder: &Binder, e: &Expr, frees: &Frees) -> T<bool> {
    let mut tr = Tr::new(m, s, d, frees, &[], ent)?;
    tr.formulas = true;
    tr.bind_top(binder);
    tr.pending_guards.clear();
    let c = tr.cond(e, &B::True)?;
    Ok(tr.pending_guards.is_empty() && c.g == B::False)
}

/// The rows (of `alias`, a row of `ent`) matching a full-text search: each word begins a word
/// of one of the fields, through their search index (`Schema::search_columns`: an FTS5 table on
/// SQLite, a GIN index per column on PostgreSQL). With the fields it reads.
pub fn fts_cond(d: Dialect, sc: &Schema, ent: &str, s: &TextSearch, alias: &str) -> T<(B, Vec<String>)> {
    if s.words.is_empty() {
        return Ok((B::True, Vec::new()));
    }
    let t = sc.entity_table(ent).ok_or("no table")?;
    let indexed = sc.search_columns(&t.name);
    let mut cols: Vec<String> = Vec::new();
    for f in &s.fields {
        let Some(fm) = t.fields.iter().find(|m| m.field == *f && !m.formula && m.columns.len() == 1) else {
            return no(format!("`{ent}.{f}` has no column"));
        };
        let c = fm.columns[0].name.clone();
        if !indexed.contains(&c) {
            return no(format!("`{ent}.{f}` is not in the search index"));
        }
        cols.push(c);
    }
    if cols.is_empty() {
        return Ok((B::False, Vec::new()));
    }
    let b = match d {
        Dialect::Sqlite => {
            // the words are letters and digits: quoted, each a prefix
            let mut m = s.words.iter().map(|w| format!("\"{w}\"*")).collect::<Vec<_>>().join(" AND ");
            if cols.len() < indexed.len() {
                // the FTS5 table's columns are `c0`, `c1`… in the index's order
                let names: Vec<String> = cols.iter().map(|c| format!("c{}", indexed.iter().position(|x| x == c).unwrap_or(0))).collect();
                m = format!("{{{}}} : ({m})", names.join(" "));
            }
            let fts = crate::schema::fts_table(&t.name);
            B::S(sql!(format!("({alias}.\"id\" IN (SELECT \"rowid\" FROM {} WHERE {} MATCH ", q(&fts), q(&fts)), Frag::p("text", Sql::Text(m)), "))"))
        }
        Dialect::Postgres => {
            let mut all = B::True;
            for w in &s.words {
                let mut any = B::False;
                for c in &cols {
                    let v = crate::schema::fts_vector_pg(&format!("{alias}.{}", q(c)));
                    any = or(any, B::S(sql!(format!("({v} @@ to_tsquery('simple'::regconfig, "), Frag::p("text", Sql::Text(format!("{w}:*"))), "))")));
                }
                all = and(all, any);
            }
            all
        }
    };
    Ok((b, s.fields.clone()))
}

/// The ids of the records of `ent` matching a full-text search, in id order (tests compare
/// the search index with the text of the rows).
pub fn search_ids_sql(d: Dialect, sc: &Schema, ent: &str, s: &TextSearch) -> T<Frag> {
    let t = sc.entity_table(ent).ok_or("no table")?;
    let (b, _) = fts_cond(d, sc, ent, s, "t0")?;
    Ok(sql!(format!("SELECT CAST(t0.\"id\" AS TEXT) FROM {} t0 WHERE ", q(&t.name)), frag(&b), " ORDER BY t0.\"id\""))
}

/// The full-text search index against the text of the rows (for tests): for words of the data
/// (and their first letters), on all the searched fields and on each alone, the ids the index
/// finds must be those whose text matches (`query::text_matches`). The problems found.
pub fn search_index_problems(conn: &mut crate::conn::Conn, schema: &Schema, m: &Model) -> Result<Vec<String>, String> {
    use quadra_runtime::query::{searchable, text_matches, words};
    let d = conn.dialect();
    let mut problems = Vec::new();
    for e in m.entities.iter().filter(|e| e.kind == EntityKind::Entity && !e.temporary && !e.is_ledger) {
        let Some(t) = schema.entity_table(&e.name) else {
            continue;
        };
        let fields: Vec<(String, String)> =
            searchable(m, &e.name).into_iter().filter_map(|f| t.fields.iter().find(|x| x.field == f && !x.formula && x.columns.len() == 1).map(|x| (f.clone(), x.columns[0].name.clone()))).collect();
        if fields.is_empty() {
            continue;
        }
        let cols: Vec<String> = fields.iter().map(|(_, c)| q(c)).collect();
        let mut rows: Vec<(u64, Vec<String>)> = conn
            .query(&format!("SELECT CAST(\"id\" AS TEXT), {} FROM {}", cols.join(", "), q(&t.name)), &[])?
            .into_iter()
            .map(|r| (id_of(&r[0]), r[1..].iter().map(|v| v.clone().unwrap_or_default()).collect()))
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        let mut probe: Vec<String> = Vec::new();
        for (_, vals) in &rows {
            for w in vals.iter().flat_map(|v| words(v)) {
                for p in [w.chars().take(1).collect::<String>(), w.clone()] {
                    if !probe.contains(&p) {
                        probe.push(p);
                    }
                }
            }
        }
        probe.truncate(40);
        let sets: Vec<Vec<usize>> = std::iter::once((0..fields.len()).collect()).chain((0..fields.len()).map(|i| vec![i])).collect();
        for w in &probe {
            for set in &sets {
                let s = TextSearch { words: vec![w.clone()], fields: set.iter().map(|i| fields[*i].0.clone()).collect() };
                let want: Vec<u64> = rows.iter().filter(|(_, v)| text_matches(&s.words, &set.iter().map(|i| v[*i].clone()).collect::<Vec<_>>())).map(|(id, _)| *id).collect();
                let f = search_ids_sql(d, schema, &e.name, &s)?;
                let got: Vec<u64> = conn.query(&f.sql, &f.params)?.into_iter().map(|r| id_of(&r[0])).collect();
                if got != want {
                    problems.push(format!("search {} {:?} for `{w}`: the index finds {got:?}, the rows {want:?}", e.name, s.fields));
                }
            }
        }
    }
    Ok(problems)
}

// ---- keys, queries --------------------------------------------------------------------------

/// How a key value is bound back from a cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PK {
    Int,
    Num,
    Text,
    Date,
    Timestamp,
}

struct KeyX {
    /// the key as ordered and compared (with its collation)
    expr: Frag,
    pk: PK,
    desc: bool,
    /// may be NULL (grid keys: none last, first when descending)
    null: bool,
}

/// What a key of a group is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GK {
    Ref,
    Text,
    Int,
    Bool,
    Date(Bucket),
    DateTime(Bucket),
}

/// A key of a group in SQL (`Tr::group_key`).
struct GKey {
    /// what is grouped by, NULL for none
    group: Frag,
    /// its value as text (`query::group_value`): a function of `group`
    text: Frag,
    /// what orders the groups, in order
    sort: Vec<Frag>,
    /// the column (a reference's id): a group's rows (`key_cond`)
    col: Frag,
    kind: GK,
}

fn pk_of(k: K) -> PK {
    match k {
        K::Int | K::Duration => PK::Int,
        K::Num => PK::Num,
        K::Date => PK::Date,
        K::DateTime => PK::Timestamp,
        _ => PK::Text,
    }
}

fn cursor_param(d: Dialect, pk: PK, v: &str) -> T<Frag> {
    Ok(match pk {
        PK::Int => Frag::p("bigint", Sql::Int(v.parse().map_err(|_| "a cursor that does not fit the order")?)),
        PK::Num => {
            let x: Decimal = v.parse().map_err(|_| "a cursor that does not fit the order")?;
            match d {
                Dialect::Postgres => Frag::p("numeric", Sql::Text(dec_text(&x))),
                Dialect::Sqlite => Frag::p("text", Sql::Text(dec_text(&x))),
            }
        }
        PK::Text => Frag::p("text", Sql::Text(v.to_string())),
        PK::Date => {
            chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| "a cursor that does not fit the order")?;
            Frag::p("date", Sql::Text(v.to_string()))
        }
        PK::Timestamp => {
            chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S").map_err(|_| "a cursor that does not fit the order")?;
            Frag::p("timestamp", Sql::Text(v.to_string()))
        }
    })
}

impl<'a> Tr<'a> {
    /// A key of the language (`sort_by`): a stored value that is never none, of one currency
    /// or unit, compared as the interpreter compares.
    fn lang_key(&mut self, binder: &Binder, e: &Expr, desc: bool) -> T<KeyX> {
        self.bind_top(binder);
        self.pending_guards.clear();
        let v = self.val(e, &B::True)?;
        if !self.pending_guards.is_empty() {
            return no("a key that may fail");
        }
        Ok(match v {
            Val::Col { sql: c, k, null: false } => {
                let expr = match k {
                    K::Num => self.num_col(&c),
                    K::Text => self.text_col(&c),
                    K::Int | K::Date | K::DateTime | K::Duration => c,
                    _ => return no(format!("a {k:?} key (the interpreter cannot compare it)")),
                };
                KeyX { expr, pk: pk_of(k), desc, null: false }
            }
            Val::Money { amt, null: false, fixed: true, .. } | Val::Qty { v: amt, null: false, fixed: true, .. } => KeyX { expr: self.num_col(&amt), pk: PK::Num, desc, null: false },
            _ => return no("a key that may be none or of several currencies or units"),
        })
    }

    /// A key of the grid: a field path, as the grid orders its cells.
    fn grid_key(&mut self, path: &[String], desc: bool) -> T<KeyX> {
        let mut cur = self.row(0);
        for (i, seg) in path.iter().enumerate() {
            let v = self.member(cur, seg, true, &B::True)?;
            if i + 1 < path.len() {
                if !matches!(v, Val::Ref { .. }) {
                    return no("a path through something that is not a record");
                }
            }
            cur = v;
        }
        let lower = |s: &Self, c: &Frag| match s.d {
            Dialect::Sqlite => sql!("lower(", c, ")"),
            Dialect::Postgres => sql!("lower(", c, " COLLATE \"C\")"),
        };
        Ok(match cur {
            Val::Col { sql: c, k, null } => match k {
                K::Int => KeyX { expr: c, pk: PK::Int, desc, null },
                K::Num => KeyX { expr: self.num_col(&c), pk: PK::Num, desc, null },
                K::Text | K::Enum => KeyX { expr: lower(self, &c), pk: PK::Text, desc, null },
                K::Date => KeyX { expr: c, pk: PK::Date, desc, null },
                K::DateTime => {
                    // shown to the minute
                    let e = match self.d {
                        Dialect::Sqlite => sql!("substr(", &c, ", 1, 16)"),
                        Dialect::Postgres => sql!("date_trunc('minute', ", &c, ")"),
                    };
                    KeyX { expr: e, pk: if self.d == Dialect::Sqlite { PK::Text } else { PK::Timestamp }, desc, null }
                }
                K::Bool => {
                    let e = sql!("(CASE WHEN ", &c, " = ", self.bool_param(true), " THEN 'true' WHEN ", &c, " = ", self.bool_param(false), " THEN 'false' END)");
                    KeyX { expr: e, pk: PK::Text, desc, null }
                }
                _ => return no(format!("ordering a {k:?} column as the grid shows it")),
            },
            Val::Money { amt, null, .. } | Val::Qty { v: amt, null, .. } => KeyX { expr: self.num_col(&amt), pk: PK::Num, desc, null },
            Val::Ref { ent, id, at, .. } => KeyX { expr: self.ref_display(&ent, &id, &at)?, pk: PK::Text, desc, null: true },
            _ => return no("ordering by this value"),
        })
    }

    /// How the grid orders a reference: by the record's number, code, name or login (the first
    /// it has) in ASCII lower case, else `entity#id`; NULL for none.
    fn ref_display(&mut self, ent: &str, id: &Frag, at: &Loc) -> T<Frag> {
        let lower = |s: &Self, c: &Frag| match s.d {
            Dialect::Sqlite => sql!("lower(", c, ")"),
            Dialect::Postgres => sql!("lower(", c, " COLLATE \"C\")"),
        };
        let e = self.m.entity(ent).ok_or("unknown entity")?;
        if e.kind != EntityKind::Entity {
            return no("a reference to a trait");
        }
        let t = self.s.entity_table(ent).ok_or("no table")?.clone();
        let mut cols: Vec<(String, ColType)> = Vec::new();
        for key in ["number", "code", "name", "login"] {
            let Some(fm) = t.fields.iter().find(|m| m.field == key && !m.formula) else {
                continue;
            };
            if fm.columns.len() != 1 || fm.columns[0].part != Part::Scalar || !matches!(fm.columns[0].ty, ColType::Text | ColType::BigInt) {
                return no("a reference shown by a value that is not text");
            }
            cols.push((fm.columns[0].name.clone(), fm.columns[0].ty));
        }
        let fallback = sql!(format!("'{}#' || CAST(", ent.to_ascii_lowercase().replace('\'', "")), id, " AS TEXT)");
        let disp = if cols.is_empty() {
            fallback
        } else {
            let a = self.alias_of(ent, id, true, at)?;
            let mut co = sql!("COALESCE(");
            for (c, ty) in cols {
                let col = if ty == ColType::BigInt { sql!(format!("CAST({a}.{} AS TEXT)", q(&c))) } else { sql!(format!("{a}.{}", q(&c))) };
                co = sql!(co, lower(self, &col), ", ");
            }
            sql!(co, fallback, ")")
        };
        let expr = match self.d {
            Dialect::Postgres => sql!("(", disp, " COLLATE \"C\")"),
            Dialect::Sqlite => disp,
        };
        Ok(sql!("(CASE WHEN ", id, " IS NULL THEN NULL ELSE ", expr, " END)"))
    }

    /// A key rows are grouped by: its value as text (`query::group_value`), and what orders
    /// the groups as the grid orders the cells (text by its ASCII lower case, a record by what
    /// it is shown by, none last), ties by the value.
    fn group_key(&mut self, key: &GroupKey) -> T<GKey> {
        self.bind_top(&Binder::This);
        let mut cur = self.row(0);
        for (i, seg) in key.path.iter().enumerate() {
            // as the grid reads a path: none through a missing reference
            let v = self.member(cur, seg, true, &B::True)?;
            if i + 1 < key.path.len() && !matches!(v, Val::Ref { .. }) {
                return no("a path through something that is not a record");
            }
            cur = v;
        }
        let lower = |s: &Self, c: &Frag| match s.d {
            Dialect::Sqlite => sql!("lower(", c, ")"),
            Dialect::Postgres => sql!("lower(", c, " COLLATE \"C\")"),
        };
        // (what is grouped by, its text): on PostgreSQL the date truncated, written once per group
        let bucketed = |s: &Self, date: Frag, b: Bucket| -> (Frag, Frag) {
            let pg = |unit: &str, fmt: &str, date: Frag| {
                let g = if unit.is_empty() { date } else { sql!(format!("date_trunc('{unit}', CAST("), date, " AS TIMESTAMP))") };
                let t = sql!("to_char(", &g, format!(", '{fmt}')"));
                (g, t)
            };
            match (s.d, b) {
                (Dialect::Sqlite, Bucket::Day) => (date.clone(), date),
                (Dialect::Sqlite, Bucket::Week) => {
                    let g = sql!("date(", &date, ", '-' || ((CAST(strftime('%w', ", &date, ") AS INTEGER) + 6) % 7) || ' days')");
                    (g.clone(), g)
                }
                (Dialect::Sqlite, Bucket::Month) => {
                    let g = sql!("substr(", date, ", 1, 7)");
                    (g.clone(), g)
                }
                (Dialect::Sqlite, Bucket::Quarter) => {
                    let g = sql!("(substr(", &date, ", 1, 4) || '-Q' || ((CAST(substr(", &date, ", 6, 2) AS INTEGER) + 2) / 3))");
                    (g.clone(), g)
                }
                (Dialect::Sqlite, Bucket::Year) => {
                    let g = sql!("substr(", date, ", 1, 4)");
                    (g.clone(), g)
                }
                (Dialect::Postgres, Bucket::Day) => pg("", "YYYY-MM-DD", date),
                (Dialect::Postgres, Bucket::Week) => pg("week", "YYYY-MM-DD", date),
                (Dialect::Postgres, Bucket::Month) => pg("month", "YYYY-MM", date),
                (Dialect::Postgres, Bucket::Quarter) => pg("quarter", "YYYY-\"Q\"Q", date),
                (Dialect::Postgres, Bucket::Year) => pg("year", "YYYY", date),
            }
        };
        if key.bucket.is_some() && !matches!(cur, Val::Col { k: K::Date | K::DateTime, .. }) {
            return no("a bucket of something that is not a date");
        }
        Ok(match cur {
            Val::Ref { ent, id, at, .. } => {
                let disp = self.ref_display(&ent, &id, &at)?;
                GKey { group: id.clone(), text: sql!("CAST(", &id, " AS TEXT)"), sort: vec![disp, id.clone()], col: id, kind: GK::Ref }
            }
            Val::Col { sql: c, k, .. } => match k {
                K::Text | K::Enum => GKey { group: c.clone(), text: c.clone(), sort: vec![lower(self, &c), self.text_col(&c)], col: c, kind: GK::Text },
                K::Int => GKey { group: c.clone(), text: sql!("CAST(", &c, " AS TEXT)"), sort: vec![c.clone()], col: c, kind: GK::Int },
                K::Bool => {
                    let t = sql!("(CASE WHEN ", &c, " = ", self.bool_param(true), " THEN 'true' WHEN ", &c, " = ", self.bool_param(false), " THEN 'false' END)");
                    GKey { group: c.clone(), text: t, sort: vec![c.clone()], col: c, kind: GK::Bool }
                }
                K::Date | K::DateTime => {
                    let date = match (self.d, k) {
                        (Dialect::Sqlite, K::DateTime) => sql!("substr(", &c, ", 1, 10)"),
                        _ => c.clone(),
                    };
                    let b = key.bucket.unwrap_or(Bucket::Day);
                    let (g, t) = bucketed(self, date, b);
                    // in date order (the text of a bucket orders as its dates)
                    let sort = vec![g.clone()];
                    GKey { group: g, text: t, sort, col: c, kind: if k == K::Date { GK::Date(b) } else { GK::DateTime(b) } }
                }
                _ => return no(format!("grouping by a {k:?} value")),
            },
            _ => return no("grouping by this value (an amount, a quantity, a collection)"),
        })
    }

    /// The rows of a group: their key has this value (None: no value).
    fn key_cond(&mut self, key: &GroupKey, value: &Option<String>) -> T<B> {
        let g = self.group_key(key)?;
        let Some(v) = value else {
            return Ok(self.is_null(&g.col));
        };
        let bad = || format!("`{v}` is not a value of `{}`", key.path.join("."));
        Ok(match g.kind {
            GK::Ref => B::S(sql!("(", &g.col, " = ", Frag::p("bigint", Sql::Int(v.parse().map_err(|_| bad())?)), ")")),
            GK::Int => B::S(sql!("(", &g.col, " = ", Frag::p("bigint", Sql::Int(v.parse().map_err(|_| bad())?)), ")")),
            GK::Text => B::S(sql!("(", &g.col, " = ", Frag::p("text", Sql::Text(v.clone())), ")")),
            GK::Bool => B::S(sql!("(", &g.col, " = ", self.bool_param(v == "true"), ")")),
            GK::Date(b) => {
                let (from, to) = b.range(v).ok_or_else(bad)?;
                B::S(sql!("(", &g.col, " >= ", Frag::p("date", Sql::Text(from.to_string())), " AND ", &g.col, " < ", Frag::p("date", Sql::Text(to.to_string())), ")"))
            }
            GK::DateTime(b) => {
                let (from, to) = b.range(v).ok_or_else(bad)?;
                let ts = |d: chrono::NaiveDate| Frag::p("timestamp", Sql::Text(format!("{d} 00:00:00")));
                B::S(sql!("(", &g.col, " >= ", ts(from), " AND ", &g.col, " < ", ts(to), ")"))
            }
        })
    }

    /// The rows matching a full-text search: each word begins a word of one of the fields
    /// (their search index: `Schema::search_columns`).
    fn search_cond(&mut self, s: &TextSearch) -> T<B> {
        let ent = self.scopes[0].entity.clone();
        let (b, read) = fts_cond(self.d, self.s, &ent, s, "t0")?;
        for f in read {
            self.fields.insert((ent.clone(), f));
        }
        Ok(b)
    }

    fn bind_top(&mut self, binder: &Binder) {
        let s = &mut self.scopes[0];
        match binder {
            Binder::Param(p) => {
                s.this = false;
                s.names = vec![p.clone()];
            }
            Binder::This => {
                s.this = true;
                s.names.clear();
            }
        }
    }
}

/// `ORDER BY` of the keys and the id.
fn order_by(d: Dialect, keys: &[KeyX], id_desc: bool) -> Frag {
    let mut f = sql!(" ORDER BY ");
    for k in keys {
        f = sql!(f, &k.expr, if k.desc { " DESC" } else { " ASC" });
        if k.null && d == Dialect::Sqlite {
            // the grid shows none last (first when descending); PostgreSQL does so by default
            f = sql!(f, if k.desc { " NULLS FIRST" } else { " NULLS LAST" });
        }
        f = sql!(f, ", ");
    }
    sql!(f, if id_desc { "t0.\"id\" DESC" } else { "t0.\"id\" ASC" })
}

/// The rows after a cursor, in the order of `keys` then the id.
fn keyset(d: Dialect, keys: &[KeyX], after: &After, id_desc: bool) -> T<B> {
    if after.keys.len() != keys.len() {
        return no("a cursor that does not fit the order");
    }
    let idp = Frag::p("bigint", Sql::Int(after.id as i64));
    let mut acc = B::S(sql!(if id_desc { "(t0.\"id\" < " } else { "(t0.\"id\" > " }, &idp, ")"));
    for (k, c) in keys.iter().zip(after.keys.iter()).rev() {
        let (gt, same) = match c {
            None => {
                if !k.null {
                    return no("a cursor that does not fit the order");
                }
                let gt = if k.desc { B::S(sql!("(", &k.expr, " IS NOT NULL)")) } else { B::False };
                (gt, B::S(sql!("(", &k.expr, " IS NULL)")))
            }
            Some(v) => {
                let p = cursor_param(d, k.pk, v)?;
                let mut gt = B::S(sql!("(", &k.expr, if k.desc { " < " } else { " > " }, &p, ")"));
                if k.null && !k.desc {
                    gt = or(gt, B::S(sql!("(", &k.expr, " IS NULL)")));
                }
                (gt, B::S(sql!("(", &k.expr, " = ", &p, ")")))
            }
        };
        acc = or(gt, and(same, acc));
    }
    // the first key bounds the range (an index scan starts there)
    if let (Some(k), Some(Some(v))) = (keys.first(), after.keys.first()) {
        if !k.null {
            let p = cursor_param(d, k.pk, v)?;
            acc = and(B::S(sql!("(", &k.expr, if k.desc { " <= " } else { " >= " }, &p, ")")), acc);
        }
    }
    Ok(acc)
}

/// A translated query: what to select from, the conditions, the flag of the rows the
/// interpreter decides.
pub struct Plan {
    from: Frag,
    cand: B,
    check: B,
    keys: Vec<KeyX>,
    id_desc: bool,
    pub residual: Vec<usize>,
    pub guarded: bool,
    pub perm_sql: bool,
    pub perm_engine: Option<String>,
    pub entities: Vec<String>,
    pub fields: Vec<(String, String)>,
    table: String,
    /// the rows are a set of ids (row permissions on SQLite): read in id order at no cost
    by_id: bool,
    /// the rows the permission lets through (or flags), without the query's filter
    perm_cand: B,
    unfiltered: bool,
    aggs: Vec<Frag>,
    agg_kinds: Vec<AggK>,
    /// what orders groups by an aggregate (a number; NULL for none or several units)
    agg_orders: Vec<Frag>,
    gkeys: Vec<GKey>,
    d: Dialect,
}

type AggK = &'static str;

/// Translate a request (no database access).
pub fn plan(d: Dialect, s: &Schema, r: &Request) -> T<Plan> {
    // the query's entity is written here too: its other records are read as committed
    let mut dirty: Vec<String> = r.dirty.clone();
    if !r.exclude.is_empty() {
        dirty.push(r.entity.to_string());
    }
    let mut tr = Tr::new(r.model, s, d, r.frees, &dirty, r.entity)?;
    tr.formulas = r.formulas;
    let mut cand = B::True;
    let mut check = B::False;
    let mut residual = Vec::new();
    for (i, c) in r.conds.iter().enumerate() {
        tr.bind_top(c.binder);
        let snapshot = (tr.scopes[0].joins.len(), tr.scopes[0].paths.clone(), tr.entities.clone(), tr.fields.clone());
        tr.pending_guards.clear();
        match tr.cond(c.expr, &B::True) {
            Ok(x) if tr.pending_guards.is_empty() => {
                let t = if c.negate { not(d, x.t) } else { x.t };
                cand = and(cand, or(t, x.g.clone()));
                check = or(check, x.g);
            }
            // a failure no condition collected (never expected): the interpreter's
            _ => {
                // nothing of what it joined stays
                tr.scopes[0].joins.truncate(snapshot.0);
                tr.scopes[0].paths = snapshot.1;
                tr.entities = snapshot.2;
                tr.fields = snapshot.3;
                residual.push(i);
            }
        }
    }
    // the lines of a record: its reference, or its set holding the record
    for (field, target) in &r.refs {
        tr.bind_top(&Binder::This);
        let row = tr.row(0);
        let t = Frag::p("bigint", Sql::Int(*target as i64));
        match tr.member(row, field, true, &B::True)? {
            Val::Ref { id, .. } => cand = and(cand, B::S(sql!("(", &id, " = ", &t, ")"))),
            Val::Coll(c @ Coll::Stored { .. }) => {
                let x = Val::Const(V::Ref { ent: std::rc::Rc::from(""), id: *target });
                cand = and(cand, tr.contains(&c, &[], &x)?.t);
            }
            _ => return no(format!("`{field}` is not a reference")),
        }
    }
    // the rows of a group (a key's value); through records the user may not all read, the
    // interpreter decides each row (it hides their values)
    let mut keys_residual = false;
    for (key, value) in &r.keys {
        if key.hidden_path {
            keys_residual = true;
            continue;
        }
        let c = tr.key_cond(key, value)?;
        cand = and(cand, c);
    }
    // a full-text search
    if let Some(srch) = &r.search {
        let c = tr.search_cond(srch)?;
        cand = and(cand, c);
    }
    // the rows the user may read (phase 2): sets of ids the database computes from the user
    let mut perm_sql = false;
    let mut perm_engine = None;
    let mut perm_cand = B::True;
    if let Some(p) = r.perm {
        let snapshot = (tr.scopes[0].joins.len(), tr.scopes[0].paths.clone(), tr.entities.clone(), tr.fields.clone());
        match tr.perm_cond(p) {
            Ok(c) => {
                perm_cand = or(c.t, c.g.clone());
                cand = and(cand, perm_cand.clone());
                check = or(check, c.g);
                perm_sql = true;
                perm_counted(true);
            }
            Err(why) => {
                tr.scopes[0].joins.truncate(snapshot.0);
                tr.scopes[0].paths = snapshot.1;
                tr.entities = snapshot.2;
                tr.fields = snapshot.3;
                perm_engine = Some(why);
                perm_counted(false);
            }
        }
    }
    let id_desc = r.id_desc;
    let mut keys = Vec::new();
    for k in &r.order {
        match k {
            Key::Lang { binder, expr, desc } => keys.push(tr.lang_key(binder, expr, *desc)?),
            Key::Grid { path, desc } => keys.push(tr.grid_key(path, *desc)?),
        }
    }
    let mut aggs = Vec::new();
    let mut agg_kinds = Vec::new();
    let mut agg_orders = Vec::new();
    let mut gkeys: Vec<GKey> = Vec::new();
    let mut within = B::True;
    if let What::Groups { keys, within: w, .. } = &r.what {
        for (n, k) in keys.iter().enumerate() {
            tr.pending_guards.clear();
            let g = tr.group_key(k)?;
            // rows whose key may fail (a formula's empty column): the interpreter groups them
            for x in std::mem::take(&mut tr.pending_guards) {
                check = or(check, x);
            }
            if let Some(Some(vals)) = w.get(n) {
                let mut any = B::False;
                let mut list = sql!("(", &g.text, " IN (");
                let mut some = false;
                for v in vals {
                    match v {
                        Some(v) => {
                            list = sql!(list, if some { ", " } else { "" }, Frag::p("text", Sql::Text(v.clone())));
                            some = true;
                        }
                        None => any = or(any, B::S(sql!("(", &g.col, " IS NULL)"))),
                    }
                }
                if some {
                    any = or(any, B::S(sql!(list, "))")));
                }
                within = and(within, any);
            }
            gkeys.push(g);
        }
    }
    let agg_list: Option<&Vec<quadra_runtime::query::Agg>> = match &r.what {
        What::Aggs(list) => Some(list),
        What::Groups { aggs, .. } => Some(aggs),
        _ => None,
    };
    if let Some(list) = agg_list {
        for a in list {
            tr.pending_guards.clear();
            let v = match &a.of {
                AggOf::Expr { binder, expr } => {
                    tr.bind_top(binder);
                    tr.val(expr, &B::True)?
                }
                AggOf::Path(path) => {
                    // as the grid reads a path: none through a missing reference
                    let mut cur = tr.row(0);
                    for seg in path {
                        cur = tr.member(cur, seg, true, &B::True)?;
                    }
                    cur
                }
            };
            // the rows where it may fail: the interpreter aggregates when there are any
            for g in std::mem::take(&mut tr.pending_guards) {
                check = or(check, g);
            }
            let (f, k, o) = agg_sql(d, &v, a.func)?;
            aggs.push(f);
            agg_kinds.push(k);
            agg_orders.push(o);
        }
    }
    let t0 = s.entity_table(r.entity).ok_or("no table")?.name.clone();
    let mut from = sql!(format!(" FROM {} t0", q(&t0)));
    for j in &tr.scopes[0].joins {
        from = sql!(from, j);
    }
    if !r.exclude.is_empty() {
        if r.exclude.len() > 5000 {
            return no("too many records written in this transaction");
        }
        let mut l = sql!("t0.\"id\" NOT IN (");
        for (i, id) in r.exclude.iter().enumerate() {
            l = sql!(l, if i > 0 { ", " } else { "" }, Frag::p("bigint", Sql::Int(*id as i64)));
        }
        cand = and(cand, B::S(sql!(l, ")")));
    }
    if let Some(ids) = &r.ids {
        if ids.len() > 5000 {
            return no("too many ids");
        }
        if ids.is_empty() {
            cand = B::False;
        } else {
            let mut l = sql!("t0.\"id\" IN (");
            for (i, id) in ids.iter().enumerate() {
                l = sql!(l, if i > 0 { ", " } else { "" }, Frag::p("bigint", Sql::Int(*id as i64)));
            }
            cand = and(cand, B::S(sql!(l, ")")));
        }
    }
    cand = and(cand, within);
    let unfiltered = cand == B::True;
    let guarded = check != B::False;
    if keys_residual {
        // (not a condition of the request: no position)
        residual.push(usize::MAX);
    }
    if !residual.is_empty() {
        check = B::True;
    }
    Ok(Plan {
        from,
        cand,
        check,
        keys,
        id_desc,
        residual,
        guarded,
        by_id: perm_sql && d == Dialect::Sqlite,
        perm_cand,
        perm_sql,
        perm_engine,
        entities: tr.entities.into_iter().collect(),
        fields: tr.fields.into_iter().collect(),
        table: t0,
        unfiltered,
        aggs,
        agg_kinds,
        agg_orders,
        gkeys,
        d,
    })
}

/// SQL of an aggregate of a value: (select list, how to read it, what orders groups by it).
fn agg_sql(d: Dialect, v: &Val, func: AggFn) -> T<(Frag, AggK, Frag)> {
    let num = |c: &Frag| match d {
        Dialect::Postgres => c.clone(),
        Dialect::Sqlite => sql!("(", c, " COLLATE quadra_dec)"),
    };
    let sum = |c: &Frag| match d {
        Dialect::Postgres => sql!("CAST(SUM(", c, ") AS TEXT)"),
        Dialect::Sqlite => sql!("quadra_dsum(", c, ")"),
    };
    let count = |c: &Frag| sql!("CAST(COUNT(", c, ") AS TEXT)");
    Ok(match v {
        Val::Col { sql: c, k, .. } => {
            let value = match (func, k) {
                (AggFn::Count, _) => sql!("CAST(NULL AS TEXT)"),
                (AggFn::Sum, K::Int | K::Duration) => sql!("CAST(SUM(", c, ") AS TEXT)"),
                (AggFn::Sum, K::Num) => sum(c),
                (AggFn::Min | AggFn::Max, K::Int | K::Date | K::DateTime | K::Duration) => sql!(if func == AggFn::Min { "CAST(MIN(" } else { "CAST(MAX(" }, c, ") AS TEXT)"),
                (AggFn::Min | AggFn::Max, K::Num) => sql!(if func == AggFn::Min { "CAST(MIN(" } else { "CAST(MAX(" }, num(c), ") AS TEXT)"),
                (AggFn::Min | AggFn::Max, K::Text) if d == Dialect::Postgres => sql!(if func == AggFn::Min { "MIN(" } else { "MAX(" }, c, " COLLATE \"C\")"),
                (AggFn::Min | AggFn::Max, K::Text) => {
                    sql!(if func == AggFn::Min { "MIN(" } else { "MAX(" }, c, ")")
                }
                _ => return no(format!("{func:?} of a {k:?} value")),
            };
            let kind = match k {
                K::Int => "int",
                K::Num => "num",
                K::Date => "date",
                K::DateTime => "datetime",
                K::Duration => "duration",
                _ => "text",
            };
            let order = match (func, k) {
                (AggFn::Count, _) => sql!("COUNT(", c, ")"),
                (AggFn::Sum, K::Num) if d == Dialect::Sqlite => {
                    sql!("(quadra_dsum(", c, ") COLLATE quadra_dec)")
                }
                (AggFn::Sum, _) => sql!("SUM(", c, ")"),
                (AggFn::Min, K::Num) => sql!("MIN(", num(c), ")"),
                (AggFn::Max, K::Num) => sql!("MAX(", num(c), ")"),
                (AggFn::Min, K::Text) if d == Dialect::Postgres => {
                    sql!("MIN(", c, " COLLATE \"C\")")
                }
                (AggFn::Max, K::Text) if d == Dialect::Postgres => {
                    sql!("MAX(", c, " COLLATE \"C\")")
                }
                (AggFn::Min, _) => sql!("MIN(", c, ")"),
                (AggFn::Max, _) => sql!("MAX(", c, ")"),
            };
            (sql!(value, ", CAST(NULL AS TEXT), '0', ", count(c)), kind, order)
        }
        Val::Money { amt, cur: unit, .. } | Val::Qty { v: amt, unit, .. } => {
            let value = match func {
                AggFn::Count => sql!("CAST(NULL AS TEXT)"),
                AggFn::Sum => sum(amt),
                AggFn::Min => sql!("CAST(MIN(", num(amt), ") AS TEXT)"),
                AggFn::Max => sql!("CAST(MAX(", num(amt), ") AS TEXT)"),
            };
            let kind = if matches!(v, Val::Money { .. }) { "money" } else { "qty" };
            let units = sql!("(COUNT(DISTINCT ", unit, ") + COALESCE(MAX(CASE WHEN ", unit, " IS NULL AND ", amt, " IS NOT NULL THEN 1 ELSE 0 END), 0))");
            let num_of = match func {
                AggFn::Count => sql!("COUNT(", amt, ")"),
                AggFn::Sum if d == Dialect::Sqlite => {
                    sql!("(quadra_dsum(", amt, ") COLLATE quadra_dec)")
                }
                AggFn::Sum => sql!("SUM(", amt, ")"),
                AggFn::Min => sql!("MIN(", num(amt), ")"),
                AggFn::Max => sql!("MAX(", num(amt), ")"),
            };
            let order = if func == AggFn::Count { num_of } else { sql!("(CASE WHEN ", &units, " > 1 THEN NULL ELSE ", num_of, " END)") };
            (sql!(value, ", MIN(", unit, "), CAST(", units, " AS TEXT), ", count(amt)), kind, order)
        }
        _ => return no("an aggregate of this value"),
    })
}

fn id_of(s: &Option<String>) -> u64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

impl Plan {
    /// The rows of a query too large to count (more than `cap`): (estimate, exact). Without a
    /// filter: SQLite counts the table (cheap), PostgreSQL gives its statistics; with one: the
    /// share of the table's id range the first `cap` + 1 matches took, extrapolated.
    fn estimate(&self, conn: &mut crate::conn::Conn, cap: u64) -> Result<(u64, bool), String> {
        let t = q(&self.table);
        if self.unfiltered {
            if self.d == Dialect::Postgres {
                let r = conn.query("SELECT CAST(CAST(c.reltuples AS BIGINT) AS TEXT) FROM pg_class c WHERE c.oid = to_regclass(?text)", &[Sql::Text(t.clone())])?;
                let n: i64 = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok()).unwrap_or(-1);
                if n > cap as i64 {
                    return Ok((n as u64, false));
                }
            }
            let r = conn.query(&format!("SELECT CAST(COUNT(*) AS TEXT) FROM {t}"), &[])?;
            return Ok((r.first().map(|r| id_of(&r[0])).unwrap_or(0), true));
        }
        if self.d == Dialect::Postgres {
            // the planner's estimate of the matching rows (its statistics, no rows read: the
            // ordered scan below reads the first `cap` + 1 matches, slow on a cold cache when
            // they are spread over joins)
            let f = sql!("EXPLAIN SELECT t0.\"id\"", &self.from, self.where_(B::True));
            let r = conn.query(&f.sql, &f.params)?;
            // the first line: `… (cost=0.00..123.45 rows=678 width=8)`
            let rows = r.first().and_then(|r| r[0].as_deref()).and_then(|l| l.split(" rows=").nth(1)).and_then(|x| x.split(' ').next()).and_then(|x| x.parse::<f64>().ok());
            if let Some(n) = rows {
                return Ok(((n.round() as u64).max(cap + 1), false));
            }
        }
        let f = sql!("SELECT CAST(MAX(q.id) AS TEXT) FROM (SELECT t0.\"id\" AS id", &self.from, self.where_(B::True), format!(" ORDER BY t0.\"id\" LIMIT {}) q", cap + 1));
        let at = conn.query(&f.sql, &f.params)?.first().map(|r| id_of(&r[0])).unwrap_or(0);
        self.extrapolate(conn, cap, at)
    }

    /// The rows of a filter whose first `cap` + 1 matches end at id `at`, extrapolated over the
    /// table's id range.
    fn extrapolate(&self, conn: &mut crate::conn::Conn, cap: u64, at: u64) -> Result<(u64, bool), String> {
        let t = q(&self.table);
        // two subqueries: each is one step of the id index
        let r = conn.query(&format!("SELECT CAST((SELECT MIN(\"id\") FROM {t}) AS TEXT), CAST((SELECT MAX(\"id\") FROM {t}) AS TEXT)"), &[])?;
        let (lo, hi) = r.first().map(|r| (id_of(&r[0]), id_of(&r[1]))).unwrap_or((0, 0));
        if at <= lo || hi <= lo {
            return Ok((cap + 1, false));
        }
        let est = ((cap + 1) as f64 * (hi - lo) as f64 / (at - lo) as f64).round() as u64;
        Ok((est.max(cap + 1), false))
    }

    fn where_(&self, extra: B) -> Frag {
        sql!(" WHERE ", frag(&and(self.cand.clone(), extra)))
    }

    fn check_col(&self) -> Frag {
        match &self.check {
            B::False => sql!("0"),
            B::True => sql!("1"),
            B::S(f) => sql!("CASE WHEN ", f, " THEN 1 ELSE 0 END"),
        }
    }

    /// Execute the plan for `what`.
    pub fn run_counted(&self, conn: &mut crate::conn::Conn, r: &Request) -> Result<Answer, String> {
        SELECTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.run(conn, r)
    }

    /// Execute the plan for `what`.
    pub fn run(&self, conn: &mut crate::conn::Conn, r: &Request) -> Result<Answer, String> {
        let mut out = Answer {
            residual: self.residual.clone(),
            guarded: self.guarded,
            perm_sql: self.perm_sql,
            perm_engine: self.perm_engine.clone(),
            entities: self.entities.clone(),
            fields: self.fields.clone(),
            ..Default::default()
        };
        match &r.what {
            What::Rows { after, limit, only_checked } => {
                let mut extra = B::True;
                if let Some(a) = after {
                    extra = keyset(self.d, &self.keys, a, self.id_desc)?;
                }
                if *only_checked {
                    extra = and(extra, self.check.clone());
                }
                let mut sel = sql!("SELECT CAST(t0.\"id\" AS TEXT), CAST(", self.check_col(), " AS TEXT)");
                for k in &self.keys {
                    sel = sql!(sel, ", CAST(", &k.expr, " AS TEXT)");
                }
                let f = sql!(sel, &self.from, self.where_(extra), order_by(self.d, &self.keys, self.id_desc), format!(" LIMIT {}", limit));
                for row in conn.query(&f.sql, &f.params)? {
                    out.rows.push(quadra_runtime::query::Row { id: id_of(&row[0]), check: row[1].as_deref() == Some("1"), keys: row[2..].to_vec() });
                }
            }
            What::Count { cap: Some(c) } if self.by_id && !self.unfiltered => {
                // the count and, beyond the cap, where the first `cap` + 1 rows end (the estimate)
                let f = sql!(
                    "SELECT CAST(COUNT(*) AS TEXT), CAST(COALESCE(SUM(q.g), 0) AS TEXT), CAST(MAX(q.i) AS TEXT) FROM (SELECT ",
                    self.check_col(),
                    " AS g, t0.\"id\" AS i",
                    &self.from,
                    self.where_(B::True),
                    format!(" ORDER BY t0.\"id\" LIMIT {}) q", c + 1)
                );
                let rows = conn.query(&f.sql, &f.params)?;
                let row = rows.first().cloned().unwrap_or_default();
                let n = id_of(row.first().unwrap_or(&None));
                out.count = Some((n, id_of(row.get(1).unwrap_or(&None))));
                if n > *c {
                    // the id range assumes the rows spread evenly: never more than the rows the
                    // user may read (the permission's set alone, counted: the set is built for
                    // the count anyway). A user whose rows are the first of the table (the
                    // regions of a logistics user) would otherwise be told the whole table.
                    let (est, exact) = self.extrapolate(conn, *c, id_of(row.get(2).unwrap_or(&None)))?;
                    let f = sql!(format!("SELECT CAST(COUNT(*) AS TEXT) FROM {} t0 WHERE ", q(&self.table)), frag(&self.perm_cand));
                    let readable = conn.query(&f.sql, &f.params)?.first().map(|r| id_of(&r[0])).unwrap_or(u64::MAX);
                    out.estimate = Some((est.min(readable).max(*c + 1), exact));
                }
            }
            What::Count { cap } => {
                let lim = cap.map(|c| format!(" LIMIT {}", c + 1)).unwrap_or_default();
                let f = sql!("SELECT CAST(COUNT(*) AS TEXT), CAST(COALESCE(SUM(q.g), 0) AS TEXT) FROM (SELECT ", self.check_col(), " AS g", &self.from, self.where_(B::True), lim, ") q");
                let rows = conn.query(&f.sql, &f.params)?;
                let row = rows.first().cloned().unwrap_or_default();
                let n = id_of(row.first().unwrap_or(&None));
                out.count = Some((n, id_of(row.get(1).unwrap_or(&None))));
                if let Some(c) = cap.filter(|c| n > *c) {
                    out.estimate = Some(self.estimate(conn, c)?);
                }
            }
            What::Groups { aggs, order, desc, offset, limit, .. } => {
                let mut by = sql!(" GROUP BY ");
                for (n, g) in self.gkeys.iter().enumerate() {
                    by = sql!(by, if n > 0 { ", " } else { "" }, &g.group);
                    for x in &g.sort {
                        by = sql!(by, ", ", x);
                    }
                }
                // how many groups, and the rows the interpreter must decide (read with the page,
                // below, when it has a row)
                let totals = |conn: &mut crate::conn::Conn| -> Result<(u64, u64), String> {
                    let f = sql!("SELECT CAST(COUNT(*) AS TEXT), CAST(COALESCE(SUM(q.f), 0) AS TEXT) FROM (SELECT SUM(", self.check_col(), ") AS f", &self.from, self.where_(B::True), &by, ") q");
                    let rows = conn.query(&f.sql, &f.params)?;
                    let row = rows.first().cloned().unwrap_or_default();
                    Ok((id_of(row.first().unwrap_or(&None)), id_of(row.get(1).unwrap_or(&None))))
                };
                if *limit == 0 {
                    out.groups_total = Some(totals(conn)?);
                    return Ok(out);
                }
                let nulls = |desc: bool| {
                    if self.d == Dialect::Sqlite {
                        if desc {
                            " NULLS FIRST"
                        } else {
                            " NULLS LAST"
                        }
                    } else {
                        ""
                    }
                };
                let mut items: Vec<Frag> = Vec::new();
                match order {
                    GroupOrder::Key => {}
                    GroupOrder::Count => items.push(sql!(if *desc { "COUNT(*) DESC" } else { "COUNT(*) ASC" })),
                    GroupOrder::Agg(i) => {
                        let o = self.agg_orders.get(*i).ok_or("no such aggregate")?;
                        // none (and several units) last either way
                        items.push(sql!(o, if *desc { " DESC NULLS LAST" } else { " ASC NULLS LAST" }));
                    }
                    GroupOrder::Ranked { by, then } => {
                        match by {
                            Some(i) => {
                                let o = self.agg_orders.get(*i).ok_or("no such aggregate")?;
                                items.push(sql!(o, if *desc { " DESC NULLS LAST" } else { " ASC NULLS LAST" }));
                            }
                            None => items.push(sql!(if *desc { "COUNT(*) DESC" } else { "COUNT(*) ASC" })),
                        }
                        let o = self.agg_orders.get(*then).ok_or("no such aggregate")?;
                        items.push(sql!(o, " ASC NULLS LAST"));
                    }
                }
                // the keys (descending only when the groups are ordered by them)
                let kdesc = *desc && *order == GroupOrder::Key;
                for g in &self.gkeys {
                    for x in &g.sort {
                        items.push(sql!(x, if kdesc { " DESC" } else { " ASC" }, nulls(kdesc)));
                    }
                }
                let mut ord = sql!(" ORDER BY ");
                for (n, x) in items.into_iter().enumerate() {
                    ord = sql!(ord, if n > 0 { ", " } else { "" }, x);
                }
                let mut sel = sql!("SELECT ");
                for (n, g) in self.gkeys.iter().enumerate() {
                    sel = sql!(sel, if n > 0 { ", " } else { "" }, "CAST(", &g.text, " AS TEXT)");
                }
                sel = sql!(sel, ", CAST(COUNT(*) AS TEXT), CAST(COALESCE(SUM(", self.check_col(), "), 0) AS TEXT)");
                for a in &self.aggs {
                    sel = sql!(sel, ", ", a);
                }
                // the number of groups and the rows flagged in all of them, with the page
                sel = sql!(sel, ", CAST(COUNT(*) OVER () AS TEXT), CAST(SUM(COALESCE(SUM(", self.check_col(), "), 0)) OVER () AS TEXT)");
                let f = sql!(sel, &self.from, self.where_(B::True), &by, ord, format!(" LIMIT {limit} OFFSET {offset}"));
                let nk = self.gkeys.len();
                let rows = conn.query(&f.sql, &f.params)?;
                match rows.first() {
                    Some(r) => {
                        let at = nk + 2 + self.aggs.len() * 4;
                        out.groups_total = Some((id_of(&r[at]), id_of(&r[at + 1])));
                    }
                    None if *offset > 0 => out.groups_total = Some(totals(conn)?),
                    None => out.groups_total = Some((0, 0)),
                }
                for row in rows {
                    let mut g = quadra_runtime::query::GroupOut { keys: row[..nk].to_vec(), count: id_of(&row[nk]), flagged: id_of(&row[nk + 1]), aggs: Vec::new() };
                    for (i, k) in self.agg_kinds.iter().enumerate() {
                        let at = nk + 2 + i * 4;
                        let get = |j: usize| row.get(at + j).cloned().flatten();
                        g.aggs.push(quadra_runtime::query::AggOut { kind: k, value: get(0), unit: get(1), units: id_of(&get(2)), rows: id_of(&get(3)) });
                    }
                    out.groups.push(g);
                }
                let _ = aggs;
            }
            What::Aggs(_) => {
                let mut sel = sql!("SELECT CAST(COUNT(*) AS TEXT), CAST(COALESCE(SUM(", self.check_col(), "), 0) AS TEXT)");
                for a in &self.aggs {
                    sel = sql!(sel, ", ", a);
                }
                let f = sql!(sel, &self.from, self.where_(B::True));
                let rows = conn.query(&f.sql, &f.params)?;
                let row = rows.first().cloned().unwrap_or_default();
                out.count = Some((id_of(row.first().unwrap_or(&None)), id_of(row.get(1).unwrap_or(&None))));
                for (i, k) in self.agg_kinds.iter().enumerate() {
                    let at = 2 + i * 4;
                    let get = |j: usize| row.get(at + j).cloned().flatten();
                    out.aggs.push(quadra_runtime::query::AggOut { kind: k, value: get(0), unit: get(1), units: id_of(&get(2)), rows: id_of(&get(3)) });
                }
            }
        }
        Ok(out)
    }
}

// ---- search -------------------------------------------------------------------------------

/// The smallest text greater than every text starting with `p` (byte order): `p` with its
/// last character incremented.
fn prefix_end(p: &str) -> Option<String> {
    let mut chars: Vec<char> = p.chars().collect();
    while let Some(last) = chars.pop() {
        let mut n = last as u32 + 1;
        // skip the surrogates
        if (0xD800..0xE000).contains(&n) {
            n = 0xE000;
        }
        if let Some(c) = char::from_u32(n) {
            chars.push(c);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// `GET /v1/search`: records whose `fields` (stored text) start with `q` (ASCII lower case),
/// then those containing it, each group ordered by the record's display text in lower case
/// and its id. Columns: id, phase (`1` prefix, `2` contains), display key.
pub fn search_sql(d: Dialect, s: &Schema, ent: &str, fields: &[String], text: &str, words: Option<&TextSearch>, after: Option<&After>, limit: usize, perm: Option<(&Model, &Perm, &Frees)>) -> T<(Frag, bool)> {
    let needle = text;
    let t = s.entity_table(ent).ok_or("no table")?;
    // the rows the user may read: in SQL when the rules translate (`g` flags the rows the
    // interpreter decides), else every row is flagged
    let mut pcond = B::True;
    let mut pcheck = B::False;
    let mut perm_sql = false;
    let mut joins = Frag::default();
    if let Some((m, p, frees)) = perm {
        let r = Tr::new(m, s, d, frees, &[], ent).and_then(|mut tr| {
            let c = tr.perm_cond(p)?;
            for j in &tr.scopes[0].joins {
                joins = sql!(&joins, j);
            }
            Ok(c)
        });
        match r {
            Ok(c) => {
                pcond = or(c.t, c.g.clone());
                pcheck = c.g;
                perm_sql = true;
                perm_counted(true);
            }
            Err(_) => {
                pcheck = B::True;
                perm_counted(false);
            }
        }
    }
    let gcol = match &pcheck {
        B::False => sql!("'0'"),
        B::True => sql!("'1'"),
        B::S(f) => sql!("CASE WHEN ", f, " THEN '1' ELSE '0' END"),
    };
    let col = |f: &str| -> T<String> {
        let fm = t.fields.iter().find(|m| m.field == f && !m.formula && m.columns.len() == 1 && m.columns[0].ty == ColType::Text).ok_or("not a text field")?;
        Ok(format!("t0.{}", q(&fm.columns[0].name)))
    };
    let lower = |c: &str| match d {
        Dialect::Sqlite => format!("lower({c})"),
        Dialect::Postgres => format!("(lower({c} COLLATE \"C\"))"),
    };
    let mut cols = Vec::new();
    for f in fields {
        cols.push(col(f)?);
    }
    // the display text: the first of number, code, name, login the record has
    let mut disp: Vec<String> = Vec::new();
    for key in crate::schema::DISPLAY_FIELDS {
        if let Some(fm) = t.fields.iter().find(|m| m.field == *key && !m.formula && m.columns.len() == 1 && matches!(m.columns[0].ty, ColType::Text | ColType::BigInt)) {
            let c = format!("t0.{}", q(&fm.columns[0].name));
            disp.push(if fm.columns[0].ty == ColType::BigInt { lower(&format!("CAST({c} AS TEXT)")) } else { lower(&c) });
        }
    }
    let key = if disp.is_empty() {
        "''".to_string()
    } else if disp.len() == 1 {
        disp[0].clone()
    } else {
        format!("COALESCE({})", disp.join(", "))
    };
    let key = match d {
        Dialect::Postgres => format!("({key} COLLATE \"C\")"),
        Dialect::Sqlite => key,
    };
    let mut prefix = B::False;
    let mut contains = B::False;
    if needle.is_empty() || cols.is_empty() {
        prefix = B::True;
    } else {
        let end = prefix_end(needle);
        for c in &cols {
            let l = lower(c);
            let mut p = sql!(format!("({l} >= "), Frag::p("text", Sql::Text(needle.to_string())));
            match &end {
                Some(e) => p = sql!(p, format!(" AND {l} < "), Frag::p("text", Sql::Text(e.clone())), ")"),
                None => p = sql!(p, ")"),
            }
            prefix = or(prefix, B::S(p));
            if words.is_none() {
                let inside = match d {
                    Dialect::Sqlite => sql!(format!("(instr({l}, "), Frag::p("text", Sql::Text(needle.to_string())), ") > 0)"),
                    Dialect::Postgres => sql!(format!("(strpos({l}, "), Frag::p("text", Sql::Text(needle.to_string())), ") > 0)"),
                };
                contains = or(contains, B::S(inside));
            }
        }
    }
    // the full-text search instead of the text contained: every word begins a word of a
    // field the user may see
    if let Some(w) = words.filter(|_| !needle.is_empty()) {
        contains = fts_cond(d, s, ent, w, "t0")?.0;
    }
    let phase = |n: &str, cond: &B, after: Option<&After>| -> T<Option<Frag>> {
        let mut w = and(cond.clone(), pcond.clone());
        if let Some(a) = after {
            let ph = a.keys.first().cloned().flatten().unwrap_or_default();
            if ph.as_str() > n {
                return Ok(None);
            }
            if ph == n {
                let k = a.keys.get(1).cloned().flatten();
                let idp = Frag::p("bigint", Sql::Int(a.id as i64));
                let after_key = match k {
                    Some(k) => {
                        let kp = Frag::p("text", Sql::Text(k));
                        B::S(sql!(format!("({key} > "), &kp, format!(" OR ({key} = "), &kp, " AND t0.\"id\" > ", &idp, format!(") OR ({key} IS NULL))")))
                    }
                    None => B::S(sql!(format!("({key} IS NULL AND t0.\"id\" > "), &idp, ")")),
                };
                w = and(w, after_key);
            }
        }
        if w == B::False {
            return Ok(None);
        }
        Ok(Some(sql!(format!("SELECT CAST(t0.\"id\" AS TEXT) AS i, '{n}' AS ph, CAST({key} AS TEXT) AS k, {key} AS o, t0.\"id\" AS n, "), &gcol, format!(" AS g FROM {} t0", q(&t.name)), &joins, " WHERE ", frag(&w))))
    };
    let mut parts: Vec<Frag> = Vec::new();
    if let Some(p) = phase("1", &prefix, after)? {
        parts.push(p);
    }
    if !needle.is_empty() && (!cols.is_empty() || words.is_some()) {
        if let Some(p) = phase("2", &and(contains, not(d, prefix.clone())), after)? {
            parts.push(p);
        }
    }
    if parts.is_empty() {
        return Ok((sql!(format!("SELECT CAST(t0.\"id\" AS TEXT), '1', '', '0' FROM {} t0 WHERE 1=0", q(&t.name))), perm_sql));
    }
    let nulls = if d == Dialect::Sqlite { " NULLS LAST" } else { "" };
    let mut f = sql!("SELECT i, ph, k, g FROM (");
    for (n, p) in parts.into_iter().enumerate() {
        // each phase on its own (an index search for the prefix), then the pages in order
        f = sql!(f, if n > 0 { " UNION ALL " } else { "" }, "SELECT * FROM (", p, format!(" ORDER BY o{nulls}, n LIMIT {limit}) q{n}"));
    }
    Ok((sql!(f, format!(") r ORDER BY ph, o{nulls}, n LIMIT {limit}")), perm_sql))
}
