//! A deploy (`quadra db migrate`, `quadra dev`, the web editor's commit) without reloading the
//! database: the schema and the rules of the model deployed last are kept in a catalog
//! (`_quadra_meta`, key `catalog`), and a deploy does only what the difference needs.
//!
//! - **Tables.** A new field is a new column, a removed one is dropped, a `rename` of a
//!   migration renames the column (and its join table, and its indexes); a change of type
//!   drops the column and fills the new one from the history. What `ALTER TABLE` cannot do on
//!   SQLite (constraints, `NOT NULL` columns) is done by copying that table only, in SQL.
//! - **Data.** New defaults are computed by the engine on the records that lack them, batch by
//!   batch (written to the history exactly as one transaction would); formulas whose definition
//!   changed are computed again; only the rules the new code adds or changes are checked on
//!   the existing records (a rule reads what it reads: unchanged rules hold already).
//!   Migrations that run code run on the engine loading on demand.
//! - **Indexes** follow the model (`Schema::indexes`): on PostgreSQL the indexes of existing
//!   tables are built `CONCURRENTLY` after the commit.
//!
//! Everything else happens in one database transaction, recorded as one signed commit, as
//! before: a deploy that fails changes nothing. A database without a catalog (created by an
//! older version) is deployed once by rebuilding its tables (`ops::deploy_full`), which writes
//! the catalog.

use crate::codec::to_json;
use crate::conn::{Conn, Dialect, Pooled, Sql};
use crate::db::{encode, materialized_formulas, CommitHasher, Database};
use crate::ops::MigrateReport;
use crate::schema::{self, Column, Index, Schema, Source as TableSource, Table};
use crate::source::{head, DbSource};
use chrono::NaiveDateTime;
use quadra_ir::{
    BinOp, Block, EntityKind, Expr, ExprKind, FieldKind, Lit, Model, StmtKind, Ty, UnOp,
};
use quadra_runtime::{Cause, Commit, Flow, Id, Interp, RuleSel, SharedSource, Store, V};
use quadra_types::Report;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;

/// The version of the catalog's format: another version is deployed by rebuilding.
pub const CATALOG_VERSION: u32 = 1;

/// Records loaded and evaluated together during a deploy (tests make it small, to go
/// through many batches).
pub static BATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2_000);

/// What the database holds of the model deployed last.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Catalog {
    pub version: u32,
    /// the projection tables, as the schema generator made them
    pub tables: Vec<Table>,
    /// the `qx_` indexes of the projection tables (names as they are in the database)
    pub indexes: Vec<Index>,
    /// entity → the keys of its rules (`check:<hash>`, `req:<field>:<hash>`, …)
    pub rules: BTreeMap<String, BTreeSet<String>>,
    /// `Entity.field` of every materialized formula → hash of its definition
    pub formulas: BTreeMap<String, String>,
    /// `Entity.field` of every stored field with a default
    pub defaults: BTreeSet<String>,
    /// trait table → the entities implementing it
    pub traits: BTreeMap<String, Vec<String>>,
    /// entity → every stored field it ever had (a field added again may have a history)
    pub ever: BTreeMap<String, BTreeSet<String>>,
}

fn sha(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn to_value<T: Serialize>(t: &T) -> J {
    serde_json::to_value(t).unwrap_or(J::Null)
}

/// The string values of a JSON tree: the names an expression mentions (with some noise).
fn strings(v: &J, out: &mut BTreeSet<String>) {
    match v {
        J::String(s) => {
            out.insert(s.clone());
        }
        J::Array(a) => a.iter().for_each(|x| strings(x, out)),
        J::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

/// Hashes of definitions that include what they mention, transitively: a check that reads a
/// formula changes when the formula does. Names are matched without their entity (a change of
/// any field called `total` changes every rule that mentions a `total`): more work, never
/// less. The hash of an item covers every definition reachable from it, so it does not depend
/// on the order items are hashed in (cycles included).
struct Deep {
    /// name → hash of its own definitions, and the names they mention
    defs: HashMap<String, (String, Vec<String>)>,
}

impl Deep {
    fn new(m: &Model) -> Deep {
        let mut raw: HashMap<String, Vec<J>> = HashMap::new();
        for e in &m.entities {
            raw.entry(e.name.clone()).or_default().push(
                json!({ "kind": to_value(&e.kind), "traits": e.traits, "ledger": e.is_ledger }),
            );
            for f in &e.fields {
                raw.entry(f.name.clone()).or_default().push(json!({
                    "ty": to_value(&f.ty), "kind": to_value(&f.kind), "formula": to_value(&f.formula), "where": to_value(&f.where_),
                    "required_when": to_value(&f.required_when), "unique": f.unique, "per_company": f.per_company, "via": f.via, "through": f.through,
                }));
            }
            for f in &e.methods {
                raw.entry(f.name.clone()).or_default().push(to_value(f));
            }
        }
        for f in &m.fns {
            raw.entry(f.name.clone()).or_default().push(to_value(f));
        }
        for x in &m.metrics {
            raw.entry(x.name.clone()).or_default().push(to_value(x));
        }
        for x in &m.consts {
            raw.entry(x.name.clone()).or_default().push(to_value(x));
        }
        for x in &m.enums {
            raw.entry(x.name.clone()).or_default().push(to_value(x));
        }
        let mut defs = HashMap::new();
        for (name, texts) in &raw {
            let mut texts: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
            texts.sort();
            let mut names = BTreeSet::new();
            for t in raw[name].iter() {
                strings(t, &mut names);
            }
            let names: Vec<String> = names
                .into_iter()
                .filter(|n| n != name && raw.contains_key(n))
                .collect();
            defs.insert(name.clone(), (sha(&texts.join("\n")), names));
        }
        Deep { defs }
    }

    fn item(&mut self, v: J) -> String {
        let mut start = BTreeSet::new();
        strings(&v, &mut start);
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = start
            .into_iter()
            .filter(|n| self.defs.contains_key(n))
            .collect();
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            for x in &self.defs[&n].1 {
                if !seen.contains(x) {
                    stack.push(x.clone());
                }
            }
        }
        let mut s = v.to_string();
        for n in &seen {
            s.push_str(&format!("\n{n}={}", self.defs[n].0));
        }
        sha(&s)
    }
}

/// A rule of an entity, as a deploy re-checks it.
#[derive(Clone, Debug)]
enum RuleRef {
    Check(usize),
    Required(String),
    Where(String),
    Record(String),
    Unique(Vec<String>, Option<Expr>),
    Frozen,
    Ledger,
}

fn optional(t: Option<&Ty>) -> bool {
    matches!(
        t,
        Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_)) | None
    )
}

/// The rules of an entity, each with its key in the catalog.
fn rules_of(m: &Model, d: &mut Deep, e: &quadra_ir::Entity) -> Vec<(String, RuleRef)> {
    let mut out = Vec::new();
    if e.is_ledger {
        for c in &e.checks {
            out.push((format!("ledger:{}", d.item(to_value(c))), RuleRef::Ledger));
        }
        return out;
    }
    for (i, c) in m.all_checks(e).into_iter().enumerate() {
        out.push((
            format!(
                "{}:{}",
                if c.warn { "warn" } else { "check" },
                d.item(json!({ "cond": to_value(&c.cond), "message": to_value(&c.message) }))
            ),
            RuleRef::Check(i),
        ));
    }
    for f in m.all_fields(e) {
        if f.formula.is_some() {
            continue;
        }
        // an optional field without `required when` is never missing
        if f.required_when.is_some()
            || (!optional(f.ty.as_ref()) && f.kind == FieldKind::Stored && !f.per_company)
        {
            let h = d.item(json!({ "ty": to_value(&f.ty), "required_when": to_value(&f.required_when), "kind": to_value(&f.kind), "per_company": f.per_company }));
            out.push((
                format!("req:{}:{h}", f.name),
                RuleRef::Required(f.name.clone()),
            ));
        }
        if let Some(w) = &f.where_ {
            out.push((
                format!(
                    "where:{}:{}",
                    f.name,
                    d.item(json!({ "ty": to_value(&f.ty), "where": to_value(w) }))
                ),
                RuleRef::Where(f.name.clone()),
            ));
        }
        let inner = match &f.ty {
            Some(Ty::Optional(i)) | Some(Ty::List(i)) | Some(Ty::Set(i)) => match i.as_ref() {
                Ty::Optional(j) => Some(j.as_ref().clone()),
                other => Some(other.clone()),
            },
            other => other.clone(),
        };
        if let Some(Ty::Path { segments, .. }) = inner {
            if let Some(re) = m
                .entity(&segments[0])
                .filter(|x| x.kind == EntityKind::Record)
            {
                let fields: Vec<J> = m.all_fields(re).iter().map(|x| to_value(x)).collect();
                let checks: Vec<J> = m.all_checks(re).iter().map(|x| to_value(x)).collect();
                out.push((
                    format!(
                        "rec:{}:{}",
                        f.name,
                        d.item(json!({ "fields": fields, "checks": checks }))
                    ),
                    RuleRef::Record(f.name.clone()),
                ));
            }
        }
    }
    for u in m.all_uniques(e) {
        out.push((
            format!(
                "unique:{}:{}",
                u.fields.join(","),
                d.item(to_value(&u.when))
            ),
            RuleRef::Unique(u.fields.clone(), u.when.clone()),
        ));
    }
    for f in m.all_fields(e).into_iter().filter(|f| f.unique) {
        out.push((
            format!("unique:{}:-", f.name),
            RuleRef::Unique(vec![f.name.clone()], None),
        ));
    }
    if let Some(fr) = &e.frozen {
        out.push((
            format!(
                "frozen:{}",
                d.item(json!({ "when": to_value(&fr.when), "live": fr.live, "except": fr.except }))
            ),
            RuleRef::Frozen,
        ));
    }
    out
}

fn concrete(m: &Model) -> impl Iterator<Item = &quadra_ir::Entity> {
    m.entities
        .iter()
        .filter(|e| e.kind == EntityKind::Entity && !e.temporary)
}

/// The fields that get their default on the records lacking them (as `migrate` always did).
fn default_fields<'m>(m: &'m Model, e: &'m quadra_ir::Entity) -> Vec<&'m quadra_ir::Field> {
    m.all_fields(e)
        .into_iter()
        .filter(|f| f.default.is_some() && f.formula.is_none() && !f.per_company)
        .collect()
}

impl Catalog {
    /// The catalog of a model and its schema (`ever`: from `before`, plus the fields now).
    pub fn of(m: &Model, schema: &Schema, before: Option<&Catalog>) -> Catalog {
        let mut d = Deep::new(m);
        let mut c = Catalog {
            version: CATALOG_VERSION,
            tables: schema.tables.clone(),
            indexes: schema.indexes.clone(),
            ..Default::default()
        };
        for e in concrete(m) {
            c.rules.insert(
                e.name.clone(),
                rules_of(m, &mut d, e).into_iter().map(|(k, _)| k).collect(),
            );
            if !e.is_ledger {
                for f in default_fields(m, e) {
                    c.defaults.insert(format!("{}.{}", e.name, f.name));
                }
                let fields: BTreeSet<String> = m
                    .all_fields(e)
                    .iter()
                    .filter(|f| f.formula.is_none())
                    .map(|f| f.name.clone())
                    .collect();
                c.ever.insert(e.name.clone(), fields);
            }
        }
        for (ent, field) in materialized_formulas(schema) {
            let Some(f) = m.entity(&ent).and_then(|e| m.find_field(e, &field)) else {
                continue;
            };
            c.formulas.insert(
                format!("{ent}.{field}"),
                d.item(json!({ "formula": to_value(&f.formula), "ty": to_value(&f.ty) })),
            );
        }
        for t in &schema.tables {
            if let TableSource::Trait(tr) = &t.source {
                c.traits.insert(
                    t.name.clone(),
                    m.implementors(tr).iter().map(|e| e.name.clone()).collect(),
                );
            }
        }
        if let Some(b) = before {
            for (e, fs) in &b.ever {
                c.ever
                    .entry(e.clone())
                    .or_default()
                    .extend(fs.iter().cloned());
            }
        }
        c
    }

    pub fn read(db: &mut Database) -> Result<Option<Catalog>, String> {
        Ok(db
            .meta("catalog")?
            .and_then(|s| serde_json::from_str::<Catalog>(&s).ok())
            .filter(|c| c.version == CATALOG_VERSION))
    }

    pub fn write(&self, db: &mut Database) -> Result<(), String> {
        db.set_meta(
            "catalog",
            &serde_json::to_string(self).map_err(|e| e.to_string())?,
        )
    }
}

// ---- the plan ---------------------------------------------------------------------------------

/// `rename E.old -> E.new` of the pending migrations, per concrete entity, in order.
fn pending_renames(m: &Model, pending: &[&Block]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for b in pending {
        for s in &b.body {
            let StmtKind::Migration { keyword, paths, .. } = &s.kind else {
                continue;
            };
            if keyword != "rename" {
                continue;
            }
            let (Some(from), Some(to)) = (paths.first(), paths.get(1)) else {
                continue;
            };
            if from.len() < 2 || to.is_empty() {
                continue;
            }
            let (ent, old, new) = (&from[0], &from[from.len() - 1], &to[to.len() - 1]);
            let names: Vec<String> = match m.entity(ent) {
                Some(e) if e.kind == EntityKind::Trait => {
                    m.implementors(ent).iter().map(|e| e.name.clone()).collect()
                }
                _ => vec![ent.clone()],
            };
            for n in names {
                out.push((n, old.clone(), new.clone()));
            }
        }
    }
    out
}

/// The name of a column of field `old` once the field is called `new`.
fn renamed_column(col: &str, old: &str, new: &str) -> Option<String> {
    if col == old {
        Some(new.to_string())
    } else if col == format!("{old}_id") {
        Some(format!("{new}_id"))
    } else {
        col.strip_prefix(&format!("{old}__"))
            .map(|rest| format!("{new}__{rest}"))
    }
}

/// Apply the renames of this deploy to the catalog of the model deployed last; returns the
/// columns and tables renamed: (table, old column, new column), (old table, new table).
#[allow(clippy::type_complexity)]
fn rename_catalog(
    c: &mut Catalog,
    renames: &[(String, String, String)],
) -> (Vec<(String, String, String)>, Vec<(String, String)>) {
    let mut cols = Vec::new();
    let mut tables = Vec::new();
    for (ent, old, new) in renames {
        for t in c.tables.iter_mut() {
            match t.source.clone() {
                TableSource::Entity(e) if e == *ent => {
                    let mut map: Vec<(String, String)> = Vec::new();
                    for fm in t.fields.iter_mut().filter(|fm| fm.field == *old) {
                        fm.field = new.clone();
                        for col in fm.columns.iter_mut() {
                            if let Some(n) = renamed_column(&col.name, old, new) {
                                map.push((col.name.clone(), n.clone()));
                                col.name = n;
                            }
                        }
                    }
                    for col in t.columns.iter_mut() {
                        if let Some((_, n)) = map.iter().find(|(o, _)| *o == col.name) {
                            if let Some(ch) = &col.check {
                                col.check = Some(ch.replacen(
                                    &format!("\"{}\"", col.name),
                                    &format!("\"{n}\""),
                                    1,
                                ));
                            }
                            col.name = n.clone();
                        }
                    }
                    for fm in t.fields.iter_mut() {
                        for col in fm.columns.iter_mut() {
                            if let Some((_, n)) = map.iter().find(|(o, _)| *o == col.name) {
                                if let Some(ch) = &col.check {
                                    col.check = Some(ch.replacen(
                                        &format!("\"{}\"", col.name),
                                        &format!("\"{n}\""),
                                        1,
                                    ));
                                }
                                col.name = n.clone();
                            }
                        }
                    }
                    for u in t.uniques.iter_mut() {
                        for x in u.iter_mut() {
                            if let Some((_, n)) = map.iter().find(|(o, _)| o == x) {
                                *x = n.clone();
                            }
                        }
                    }
                    for i in c.indexes.iter_mut().filter(|i| i.table == t.name) {
                        for x in i.columns.iter_mut() {
                            if let Some((_, n)) = map.iter().find(|(o, _)| o == x) {
                                *x = n.clone();
                            }
                        }
                    }
                    for (o, n) in map {
                        cols.push((t.name.clone(), o, n));
                    }
                }
                TableSource::Join(e, f) | TableSource::PerCompany(e, f)
                    if e == *ent && f == *old =>
                {
                    let from = t.name.clone();
                    let to = format!("{}__{new}", schema::snake(&e));
                    t.source = match &t.source {
                        TableSource::Join(..) => TableSource::Join(e.clone(), new.clone()),
                        _ => TableSource::PerCompany(e.clone(), new.clone()),
                    };
                    for fm in t.fields.iter_mut() {
                        fm.field = new.clone();
                    }
                    t.name = to.clone();
                    for i in c.indexes.iter_mut().filter(|i| i.table == from) {
                        i.table = to.clone();
                    }
                    tables.push((from, to));
                }
                _ => {}
            }
        }
        if let Some(keys) = c.rules.get_mut(ent) {
            let renamed: BTreeSet<String> = keys
                .iter()
                .map(|k| {
                    let parts: Vec<&str> = k.splitn(3, ':').collect();
                    match parts.as_slice() {
                        [kind @ ("req" | "where" | "rec"), f, h] if f == old => {
                            format!("{kind}:{new}:{h}")
                        }
                        ["unique", fs, h] => {
                            let fs: Vec<&str> = fs
                                .split(',')
                                .map(|f| if f == old { new.as_str() } else { f })
                                .collect();
                            format!("unique:{}:{h}", fs.join(","))
                        }
                        _ => k.clone(),
                    }
                })
                .collect();
            *keys = renamed;
        }
        let from = format!("{ent}.{old}");
        if let Some(h) = c.formulas.remove(&from) {
            c.formulas.insert(format!("{ent}.{new}"), h);
        }
        if c.defaults.remove(&from) {
            c.defaults.insert(format!("{ent}.{new}"));
        }
        c.ever.entry(ent.clone()).or_default().insert(new.clone());
    }
    (cols, tables)
}

/// What a deploy does on the records of one entity, batch by batch.
#[derive(Default, Debug, Clone)]
struct Pass {
    /// fields that get their default where the record lacks them
    defaults: Vec<String>,
    /// stored fields whose columns are filled again from the history
    refill: BTreeSet<String>,
    /// materialized formulas computed again on every record
    formulas: BTreeSet<String>,
    /// rules checked on every record
    sel: RuleSel,
    /// new uniqueness rules: checked over every record
    uniques: Vec<(Vec<String>, Option<Expr>)>,
    /// `frozen when` changed
    refreeze: bool,
}

impl Pass {
    fn is_empty(&self) -> bool {
        self.defaults.is_empty()
            && self.refill.is_empty()
            && self.formulas.is_empty()
            && self.sel.is_empty()
            && self.uniques.is_empty()
            && !self.refreeze
    }
}

/// The change of one table.
#[derive(Debug)]
enum TableOp {
    /// a new table (trait tables are filled from the records)
    Create(Table),
    Drop(String),
    /// an entity table that changed
    Alter(Alter),
    /// a ledger table that changed: rebuilt from `_quadra_ledger_rows`
    Ledger(Table),
    /// the implementors of a trait changed: filled again
    Trait(Table),
}

#[derive(Debug)]
struct Alter {
    table: Table,
    /// columns as they are in the database, renames applied
    old: Table,
    drop: Vec<String>,
    add: Vec<Column>,
    /// kept columns whose constraints changed, and different `UNIQUE`s
    constraints: bool,
}

struct Plan {
    tables: Vec<TableOp>,
    passes: BTreeMap<String, Pass>,
    /// entity → fields a record loaded during the deploy gets its default for
    defaults: HashMap<String, Vec<String>>,
}

/// Why a deploy rebuilds every table instead.
pub(crate) struct Fallback(pub String);

fn same_col(a: &Column, b: &Column) -> bool {
    a.name == b.name && a.ty == b.ty && a.part == b.part
}

fn same_constraints(a: &Column, b: &Column) -> bool {
    a.not_null == b.not_null && a.check == b.check && a.references == b.references
}

fn make_plan(m: &Model, schema: &Schema, old: &Catalog, new: &Catalog) -> Result<Plan, Fallback> {
    let mut tables = Vec::new();
    let mut passes: BTreeMap<String, Pass> = BTreeMap::new();
    let mut d = Deep::new(m);
    // tables
    for t in &schema.tables {
        let Some(o) = old.tables.iter().find(|x| x.name == t.name) else {
            match &t.source {
                TableSource::Entity(e) if old.ever.contains_key(e) => {
                    return Err(Fallback(format!(
                        "the entity {e} is back: its records are projected again"
                    )))
                }
                TableSource::Join(e, f) | TableSource::PerCompany(e, f)
                    if old.ever.get(e).is_some_and(|s| s.contains(f))
                        && old
                            .tables
                            .iter()
                            .any(|x| matches!(&x.source, TableSource::Entity(y) if y == e)) =>
                {
                    return Err(Fallback(format!(
                        "{e}.{f} is back: its values are projected again"
                    )))
                }
                _ => {}
            }
            tables.push(TableOp::Create(t.clone()));
            continue;
        };
        match (&t.source, &o.source) {
            (TableSource::Entity(e), TableSource::Entity(_)) => {
                let pass = passes.entry(e.clone()).or_default();
                let mut alter = Alter {
                    table: t.clone(),
                    old: o.clone(),
                    drop: Vec::new(),
                    add: Vec::new(),
                    constraints: false,
                };
                let mut kept: BTreeSet<String> = [
                    "id",
                    "_created_tx",
                    "_created_at",
                    "_created_by",
                    "_frozen_tx",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                for fm in &t.fields {
                    let ofm = o.fields.iter().find(|x| x.field == fm.field);
                    let same = ofm.is_some_and(|x| {
                        x.formula == fm.formula
                            && x.columns.len() == fm.columns.len()
                            && x.columns
                                .iter()
                                .zip(&fm.columns)
                                .all(|(a, b)| same_col(a, b))
                    });
                    if same {
                        kept.extend(fm.columns.iter().map(|c| c.name.clone()));
                        continue;
                    }
                    if fm.formula {
                        pass.formulas.insert(fm.field.clone());
                    } else if ofm.is_some()
                        || old.ever.get(e).is_some_and(|s| s.contains(&fm.field))
                    {
                        // another type, or a field added again: its values come from the history
                        pass.refill.insert(fm.field.clone());
                    }
                }
                for c in &o.columns {
                    if !kept.contains(&c.name) {
                        alter.drop.push(c.name.clone());
                    }
                }
                for c in &t.columns {
                    if !kept.contains(&c.name) {
                        alter.add.push(c.clone());
                    }
                }
                alter.constraints = o.uniques != t.uniques
                    || t.columns
                        .iter()
                        .filter(|c| kept.contains(&c.name))
                        .any(|c| {
                            o.column(&c.name)
                                .map_or(true, |oc| !same_constraints(oc, c))
                        });
                if !alter.drop.is_empty() || !alter.add.is_empty() || alter.constraints {
                    tables.push(TableOp::Alter(alter));
                }
            }
            (TableSource::Ledger(_), TableSource::Ledger(_)) => {
                if serde_json::to_string(t).ok() != serde_json::to_string(o).ok() {
                    tables.push(TableOp::Ledger(t.clone()));
                }
            }
            (TableSource::Trait(_), TableSource::Trait(_)) => {
                if new.traits.get(&t.name) != old.traits.get(&t.name) {
                    tables.push(TableOp::Trait(t.clone()));
                }
            }
            (TableSource::Join(..), TableSource::Join(..))
            | (TableSource::PerCompany(..), TableSource::PerCompany(..)) => {
                if serde_json::to_string(&t.columns).ok() != serde_json::to_string(&o.columns).ok()
                {
                    // a join table of an older version, without the position of each reference:
                    // the column is added (empty: those records are read from their history
                    // until written again, their `version_tx` is 0)
                    let ord_added = matches!(t.source, TableSource::Join(..))
                        && t.columns.len() == o.columns.len() + 1
                        && t.columns.last().is_some_and(|c| c.name == "ord")
                        && serde_json::to_string(&t.columns[..o.columns.len()]).ok()
                            == serde_json::to_string(&o.columns).ok();
                    if !ord_added {
                        return Err(Fallback(format!("the table {} changed", t.name)));
                    }
                    tables.push(TableOp::Alter(Alter {
                        table: t.clone(),
                        old: o.clone(),
                        drop: Vec::new(),
                        add: vec![t.columns.last().unwrap().clone()],
                        constraints: false,
                    }));
                }
            }
            _ => return Err(Fallback(format!("the table {} changed kind", t.name))),
        }
    }
    for o in &old.tables {
        if !schema.tables.iter().any(|t| t.name == o.name) {
            tables.push(TableOp::Drop(o.name.clone()));
        }
    }
    // rules, formulas, defaults
    let mut defaults: HashMap<String, Vec<String>> = HashMap::new();
    for e in concrete(m) {
        let before = old.rules.get(&e.name).cloned().unwrap_or_default();
        let rules = rules_of(m, &mut d, e);
        let warn_keys = |ks: &mut dyn Iterator<Item = &String>| -> BTreeSet<String> {
            ks.filter(|k| k.starts_with("warn:") || k.starts_with("rec:"))
                .cloned()
                .collect()
        };
        let warnings_changed =
            warn_keys(&mut rules.iter().map(|(k, _)| k)) != warn_keys(&mut before.iter());
        let pass = passes.entry(e.name.clone()).or_default();
        pass.sel.warnings = warnings_changed;
        for (k, r) in rules {
            if before.contains(&k) {
                continue;
            }
            match r {
                RuleRef::Check(i) => {
                    pass.sel.checks.insert(i);
                }
                RuleRef::Required(f) => {
                    pass.sel.required.insert(f);
                }
                RuleRef::Where(f) => {
                    pass.sel.wheres.insert(f);
                }
                RuleRef::Record(f) => {
                    pass.sel.records.insert(f);
                }
                RuleRef::Unique(fs, w) => pass.uniques.push((fs, w)),
                RuleRef::Frozen => pass.refreeze = true,
                RuleRef::Ledger => {
                    return Err(Fallback(format!("a rule of the ledger {} changed", e.name)))
                }
            }
        }
        if e.is_ledger {
            continue;
        }
        for f in default_fields(m, e) {
            if !old.defaults.contains(&format!("{}.{}", e.name, f.name)) {
                pass.defaults.push(f.name.clone());
                defaults
                    .entry(e.name.clone())
                    .or_default()
                    .push(f.name.clone());
            }
        }
    }
    for (key, h) in &new.formulas {
        if old.formulas.get(key) != Some(h) {
            let (ent, field) = key.split_once('.').unwrap_or_default();
            passes
                .entry(ent.to_string())
                .or_default()
                .formulas
                .insert(field.to_string());
        }
    }
    passes.retain(|ent, p| !p.is_empty() && schema.entity_table(ent).is_some());
    Ok(Plan {
        tables,
        passes,
        defaults,
    })
}

// ---- running it -------------------------------------------------------------------------------

/// Rows of updates collected per (table, columns) and applied with one `UPDATE … FROM` per
/// group and batch.
#[derive(Default)]
struct Stager {
    groups: BTreeMap<(String, Vec<String>), Vec<Vec<Sql>>>,
    tables: HashMap<(String, Vec<String>), String>,
}

impl Stager {
    fn add(&mut self, table: &str, cols: Vec<String>, row: Vec<Sql>) {
        self.groups
            .entry((table.to_string(), cols))
            .or_default()
            .push(row);
    }

    fn flush(&mut self, conn: &mut Conn) -> Result<(), String> {
        let groups = std::mem::take(&mut self.groups);
        for ((table, cols), rows) in groups {
            let key = (table.clone(), cols.clone());
            let n = self.tables.len();
            let stage = match self.tables.get(&key) {
                Some(s) => s.clone(),
                None => {
                    let s = format!("_qx_stage_{n}");
                    let list: Vec<String> = std::iter::once("id".to_string())
                        .chain(cols.iter().cloned())
                        .map(|c| format!("\"{c}\""))
                        .collect();
                    let sql = match conn.dialect() {
                        Dialect::Sqlite => format!(
                            "CREATE TEMP TABLE \"{s}\" AS SELECT {} FROM \"{table}\" WHERE 0",
                            list.join(", ")
                        ),
                        Dialect::Postgres => format!(
                            "CREATE TEMP TABLE \"{s}\" AS SELECT {} FROM \"{table}\" WITH NO DATA",
                            list.join(", ")
                        ),
                    };
                    conn.batch(&sql)?;
                    self.tables.insert(key, s.clone());
                    s
                }
            };
            let names: Vec<&str> = std::iter::once("id")
                .chain(cols.iter().map(String::as_str))
                .collect();
            conn.bulk_insert(&stage, &names, &mut rows.into_iter())?;
            if conn.dialect() == Dialect::Postgres {
                // the planner then joins by the primary key instead of scanning the table
                conn.batch(&format!("ANALYZE \"{stage}\""))?;
            }
            let set: Vec<String> = cols
                .iter()
                .map(|c| format!("\"{c}\" = s.\"{c}\""))
                .collect();
            conn.batch(&format!("UPDATE \"{table}\" SET {} FROM \"{stage}\" AS s WHERE \"{table}\".\"id\" = s.\"id\"", set.join(", ")))?;
            conn.batch(&format!("DELETE FROM \"{stage}\""))?;
        }
        Ok(())
    }

    fn drop_all(&mut self, conn: &mut Conn) -> Result<(), String> {
        for s in self.tables.values() {
            conn.batch(&format!("DROP TABLE IF EXISTS \"{s}\""))?;
        }
        self.tables.clear();
        Ok(())
    }
}

/// The records of some entities, in increasing id order, a batch at a time (a merge of one
/// index scan per entity).
struct Cursors {
    list: Vec<(String, i64, VecDeque<Id>, bool)>,
    base: u64,
}

impl Cursors {
    fn new(entities: impl Iterator<Item = String>, base: u64) -> Cursors {
        Cursors {
            list: entities.map(|e| (e, 0, VecDeque::new(), false)).collect(),
            base,
        }
    }

    fn next(&mut self, conn: &mut Conn, n: usize) -> Result<Vec<(Id, String)>, String> {
        let mut out = Vec::new();
        while out.len() < n {
            for (ent, last, buf, done) in self.list.iter_mut() {
                if buf.is_empty() && !*done {
                    let rows = conn.query(
                        "SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text AND \"id\" > ?bigint AND \"created_tx\" <= ?bigint ORDER BY \"_quadra_records\".\"entity\", \"_quadra_records\".\"id\" LIMIT 5000",
                        &[Sql::Text(ent.clone()), Sql::Int(*last), Sql::Int(self.base as i64)],
                    )?;
                    if rows.is_empty() {
                        *done = true;
                    }
                    for r in rows {
                        let id: i64 = r[0].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                        *last = id;
                        buf.push_back(id as Id);
                    }
                }
            }
            let Some(i) = (0..self.list.len())
                .filter(|i| !self.list[*i].2.is_empty())
                .min_by_key(|i| self.list[*i].2[0])
            else {
                break;
            };
            let id = self.list[i].2.pop_front().unwrap();
            out.push((id, self.list[i].0.clone()));
        }
        Ok(out)
    }
}

/// `required` / `required when` of a field as conditions that hold on the records keeping
/// it (`f != none`, `f != none or not (when)`); None when the field's value is a collection
/// or per company (the batches check those).
fn required_rule(m: &Model, e: &quadra_ir::Entity, f: &quadra_ir::Field) -> Option<Vec<Expr>> {
    if f.per_company
        || f.kind != FieldKind::Stored
        || matches!(f.ty, Some(Ty::List(_)) | Some(Ty::Set(_)) | None)
        || schema::is_inverse(m, e, f)
    {
        return None;
    }
    let sp = f.span.clone();
    let present = Expr::new(
        ExprKind::Binary {
            op: BinOp::Ne,
            lhs: Box::new(Expr::new(ExprKind::Name(f.name.clone()), sp.clone())),
            rhs: Box::new(Expr::new(ExprKind::Lit(Lit::None), sp.clone())),
        },
        sp.clone(),
    );
    Some(match &f.required_when {
        Some(c) => vec![Expr::new(
            ExprKind::Binary {
                op: BinOp::Or,
                lhs: Box::new(present),
                rhs: Box::new(Expr::new(
                    ExprKind::Unary {
                        op: UnOp::Not,
                        expr: Box::new(c.clone()),
                    },
                    sp.clone(),
                )),
            },
            sp,
        )],
        None if optional(f.ty.as_ref()) => Vec::new(),
        None => vec![present],
    })
}

/// The `where` of a stored single-valued field as a condition on the record (`f == none or
/// (f > 0)`, the lambda's parameter replaced by the field); None when it cannot be written
/// so (a collection, a parameter shadowed by a nested lambda).
fn where_rule(m: &Model, e: &quadra_ir::Entity, f: &quadra_ir::Field) -> Option<Expr> {
    if f.per_company
        || f.kind != FieldKind::Stored
        || matches!(f.ty, Some(Ty::List(_)) | Some(Ty::Set(_)) | None)
        || schema::is_inverse(m, e, f)
    {
        return None;
    }
    let sp = f.span.clone();
    let name = || Expr::new(ExprKind::Name(f.name.clone()), sp.clone());
    let bin = |op: BinOp, a: Expr, b: Expr| {
        Expr::new(
            ExprKind::Binary {
                op,
                lhs: Box::new(a),
                rhs: Box::new(b),
            },
            sp.clone(),
        )
    };
    let holds = match f.where_.as_ref()? {
        quadra_ir::Where::Compare(op, x) => bin(*op, name(), x.clone()),
        quadra_ir::Where::Expr(x) => match &x.kind {
            ExprKind::Lambda { params, body } if params.len() == 1 => {
                subst(body, &params[0], &f.name)?
            }
            ExprKind::Lambda { .. } => return None,
            ExprKind::Binary {
                op: BinOp::Range,
                lhs,
                rhs,
            } => bin(
                BinOp::And,
                bin(BinOp::Ge, name(), (**lhs).clone()),
                bin(BinOp::Le, name(), (**rhs).clone()),
            ),
            _ => x.clone(),
        },
    };
    Some(bin(
        BinOp::Or,
        bin(
            BinOp::Eq,
            name(),
            Expr::new(ExprKind::Lit(Lit::None), sp.clone()),
        ),
        holds,
    ))
}

/// `e` with the name `param` replaced by the field `field` of the record; None when a nested
/// lambda binds either name, or `e` has statements.
fn subst(e: &Expr, param: &str, field: &str) -> Option<Expr> {
    let s = |x: &Expr| subst(x, param, field);
    let b = |x: &Expr| s(x).map(Box::new);
    let arg = |a: &quadra_ir::Arg| {
        Some(quadra_ir::Arg {
            name: a.name.clone(),
            value: s(&a.value)?,
            spread: a.spread,
        })
    };
    let kind = match &e.kind {
        ExprKind::Name(n) if n == param => ExprKind::Name(field.to_string()),
        ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Name(_) | ExprKind::SelfRef => {
            e.kind.clone()
        }
        ExprKind::FString(parts) => ExprKind::FString(
            parts
                .iter()
                .map(|p| match p {
                    quadra_ir::FPart::Text(t) => Some(quadra_ir::FPart::Text(t.clone())),
                    quadra_ir::FPart::Expr(x, f) => Some(quadra_ir::FPart::Expr(s(x)?, f.clone())),
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        ExprKind::Field {
            base,
            name,
            optional,
        } => ExprKind::Field {
            base: b(base)?,
            name: name.clone(),
            optional: *optional,
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: b(callee)?,
            args: args.iter().map(arg).collect::<Option<Vec<_>>>()?,
        },
        ExprKind::Index { base, index } => ExprKind::Index {
            base: b(base)?,
            index: b(index)?,
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: b(expr)?,
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: b(lhs)?,
            rhs: b(rhs)?,
        },
        ExprKind::If {
            cond,
            then,
            otherwise,
        } => ExprKind::If {
            cond: b(cond)?,
            then: b(then)?,
            otherwise: b(otherwise)?,
        },
        ExprKind::Lambda { params, body } if !params.iter().any(|p| p == param || p == field) => {
            ExprKind::Lambda {
                params: params.clone(),
                body: std::sync::Arc::new(s(body)?),
            }
        }
        ExprKind::List(items) => ExprKind::List(items.iter().map(arg).collect::<Option<Vec<_>>>()?),
        ExprKind::Set(items) => ExprKind::Set(items.iter().map(s).collect::<Option<Vec<_>>>()?),
        _ => return None,
    };
    Some(Expr::new(kind, e.span.clone()))
}

fn flow(prefix: &str, e: Flow) -> String {
    match e {
        Flow::Fail(m) if prefix.is_empty() => m,
        Flow::Fail(m) => format!("{prefix}: {m}"),
        Flow::Return(_) => format!("{prefix}: unexpected return"),
        Flow::Conflict(c) => format!(
            "{prefix}: changed by others in the meantime: {}",
            c.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

const NOT_FIT: &str = "the existing data does not satisfy the new model";

/// The engine over the database inside the deploy's transaction, and what the deploy wrote.
struct Run<'m> {
    m: &'m Model,
    schema: Rc<Schema>,
    db: Database,
    src: Rc<RefCell<DbSource>>,
    interp: Interp<'m>,
    tx: u64,
    plan: Plan,
    /// records kept in memory to the end (written by migrations, created, written outside
    /// their batch): written to the tables with `project_record`
    overlay: BTreeSet<Id>,
    /// records whose history entries were written already
    emitted: HashSet<Id>,
    stager: Stager,
    hist: Vec<Vec<Sql>>,
    ord: i64,
    missing: BTreeMap<String, Id>,
    uniques: HashMap<String, HashMap<String, Id>>,
    /// rules and formulas that read a whole entity written by the batches: evaluated on every
    /// record once the batches are done
    later: BTreeMap<String, Pass>,
    new_defaults: usize,
}

impl<'m> Run<'m> {
    /// Statements of the deploy on its connection (held by the source while the engine runs).
    fn conn(&self) -> std::cell::RefMut<'_, DbSource> {
        self.src.borrow_mut()
    }

    fn with_db<T>(
        &mut self,
        f: impl FnOnce(&mut Database) -> Result<T, String>,
    ) -> Result<T, String> {
        self.src.borrow_mut().swap_conn(&mut self.db.conn);
        let r = f(&mut self.db);
        self.src.borrow_mut().swap_conn(&mut self.db.conn);
        r
    }

    fn fail(&self, e: Flow) -> String {
        if !self.interp.violations.is_empty() {
            return self.interp.violations_report();
        }
        flow(NOT_FIT, e)
    }

    /// Migrations, on the engine loading on demand; what they write stays in memory.
    fn migrations(
        &mut self,
        pending: &[&'m Block],
        root: &Path,
        scripts: &mut Vec<(String, Vec<String>)>,
    ) -> Result<(), String> {
        for b in pending {
            let lines = self
                .interp
                .run_migration(b, Some(root))
                .map_err(|e| Flow::Fail(flow(&format!("migration {}", b.name), e)))
                .map_err(|e| self.fail(e))?;
            scripts.push((b.name.clone(), lines));
        }
        // the records in memory lacking a new default get it (those loaded from now on too)
        let ids: Vec<Id> = self.interp.store.records.keys().copied().collect();
        for id in ids {
            let r = self.interp.store.records.get_mut(&id).unwrap();
            if let Some(fields) = self.plan.defaults.get(&*r.ent) {
                for f in fields {
                    if !r.fields.contains_key(f) && !r.pending_defaults.contains(f) {
                        r.pending_defaults.push(f.clone());
                        self.new_defaults += 1;
                    }
                }
            }
        }
        self.interp.store.load_defaults = self.plan.defaults.clone();
        let i = &mut self.interp;
        let r = i.settle().and_then(|_| i.thaw_records());
        r.map_err(|e| self.fail(e))?;
        match self.interp.check_all() {
            Ok(()) => {}
            // broken rules are reported with those the batches find
            Err(_) if !self.interp.violations.is_empty() => {}
            Err(e) => return Err(self.fail(e)),
        }
        self.interp.freeze().map_err(|e| self.fail(e))?;
        self.interp.materialize_formulas();
        let mut keep: BTreeSet<Id> = self.interp.store.dirty.iter().copied().collect();
        keep.extend(self.interp.store.materialized.keys().map(|(id, _)| *id));
        keep.extend(self.interp.store.thawed.iter().copied());
        self.overlay = keep;
        let overlay = self.overlay.clone();
        self.interp.store.evict(&|id| overlay.contains(&id));
        self.interp.store.dirty.clear();
        self.interp.store.thawed.clear();
        Ok(())
    }

    /// The new rules over every record in SQL, when the deploy writes nothing (no default,
    /// refill, formula, migration): `check`s, `required` and `required when`, the `where` of
    /// fields (each one query, `SELECT … WHERE NOT (rule)` through the translator,
    /// `Interp::sql_matching`; flagged rows decided by the interpreter), or new `unique`s
    /// without `when` (`GROUP BY … HAVING COUNT(*) > 1`, a page at a time). Only the records
    /// that may break them are loaded and checked by the engine, in id order as the batches
    /// go, which builds the same report as checking every record (same order, same first
    /// five, same count; the first duplicate of a key). A rule that does not translate, a
    /// `warn`, new rules and new uniques together (the batches interleave them), any other
    /// work: None, every record is checked by the engine, batch by batch.
    fn sql_checks(
        &mut self,
        passes: &BTreeMap<String, Pass>,
        steps: &mut Vec<String>,
    ) -> Result<Option<()>, String> {
        let only_rules = |p: &Pass| {
            p.defaults.is_empty()
                && p.refill.is_empty()
                && p.formulas.is_empty()
                && !p.refreeze
                && !p.sel.all
                && !p.sel.warnings
                && p.sel.records.is_empty()
        };
        if passes.is_empty() || !passes.values().all(only_rules) {
            return Ok(None);
        }
        let rules = passes.values().any(|p| {
            !p.sel.checks.is_empty() || !p.sel.required.is_empty() || !p.sel.wheres.is_empty()
        });
        let uniques = passes.values().any(|p| !p.uniques.is_empty());
        if rules == uniques {
            return Ok(None);
        }
        if uniques {
            return self.sql_uniques(passes, steps);
        }
        let m = self.m;
        // what SQL reads is the deploy's own transaction: settled, since nothing was written
        self.src.borrow_mut().in_tx = false;
        let mut found: Vec<(String, Vec<Id>)> = Vec::new();
        let mut ok = true;
        'ents: for (ent, p) in passes {
            let Some(e) = m.entity(ent) else {
                ok = false;
                break;
            };
            // each rule as a condition that holds on the records that keep it
            let mut conds: Vec<Expr> = Vec::new();
            let checks = m.all_checks(e);
            for &ci in &p.sel.checks {
                match checks.get(ci) {
                    Some(c) if !c.warn => conds.push(c.cond.clone()),
                    _ => {
                        ok = false;
                        break 'ents;
                    }
                }
            }
            for f in m.all_fields(e) {
                if p.sel.required.contains(&f.name) {
                    match required_rule(m, e, f) {
                        Some(c) => conds.extend(c),
                        None => {
                            ok = false;
                            break 'ents;
                        }
                    }
                }
                if p.sel.wheres.contains(&f.name) && f.where_.is_some() {
                    match where_rule(m, e, f) {
                        Some(c) => conds.push(c),
                        None => {
                            ok = false;
                            break 'ents;
                        }
                    }
                }
            }
            let mut ids: BTreeSet<Id> = BTreeSet::new();
            for cond in &conds {
                let this = quadra_runtime::query::Binder::This;
                let mut spec = quadra_runtime::Spec::new(ent);
                spec.conds = vec![quadra_runtime::query::Cond {
                    binder: &this,
                    expr: cond,
                    negate: true,
                }];
                match self
                    .interp
                    .sql_matching(&spec, &mut quadra_runtime::Ctx::default())
                {
                    Ok(Some(v)) => ids.extend(v),
                    _ => {
                        ok = false;
                        break 'ents;
                    }
                }
            }
            found.push((ent.clone(), ids.into_iter().collect()));
        }
        self.src.borrow_mut().in_tx = true;
        self.interp.store.evict(&|_| false);
        if !ok {
            return Ok(None);
        }
        // in id order across the entities, as the batches go
        let mut all: Vec<(Id, String)> = found
            .iter()
            .flat_map(|(ent, ids)| ids.iter().map(move |id| (*id, ent.clone())))
            .collect();
        all.sort();
        for chunk in all.chunks(BATCH.load(std::sync::atomic::Ordering::Relaxed).max(1)) {
            let ids: Vec<Id> = chunk.iter().map(|(id, _)| *id).collect();
            self.interp.store.prefetch(&ids);
            let mut missing = Vec::new();
            for (id, ent) in chunk {
                let e = m.entity(ent).unwrap();
                self.interp
                    .check_record_sel(e, *id, Some(&passes[ent].sel), &mut missing)
                    .map_err(|err| self.fail(err))?;
            }
            for (id, msg) in missing {
                let at = self.missing.entry(msg).or_insert(id);
                *at = (*at).min(id);
            }
            self.interp.store.evict(&|_| false);
        }
        for (ent, p) in passes {
            let n = p.sel.checks.len() + p.sel.required.len() + p.sel.wheres.len();
            steps.push(format!(
                "every {ent}: {n} new or changed rule{} (in SQL)",
                if n == 1 { "" } else { "s" }
            ));
        }
        Ok(Some(()))
    }

    /// New `unique`s without `when`, in SQL: the records whose key the table holds more than
    /// once (`GROUP BY … HAVING COUNT(*) > 1`), a page at a time in id order, keyed by the
    /// engine as the batches key every record: the first record whose key an earlier one has
    /// fails the deploy, with the same message.
    fn sql_uniques(
        &mut self,
        passes: &BTreeMap<String, Pass>,
        steps: &mut Vec<String>,
    ) -> Result<Option<()>, String> {
        // (record, position of the unique among its entity's): the first duplicate
        let mut first: Option<(Id, usize, String)> = None;
        let mut plans: Vec<(String, usize, Vec<String>, Vec<String>, String)> = Vec::new();
        for (ent, p) in passes {
            let Some(t) = self.schema.entity_table(ent) else {
                return Ok(None);
            };
            for (n, (fields, when)) in p.uniques.iter().enumerate() {
                if when.is_some() {
                    return Ok(None);
                }
                let mut cols = Vec::new();
                for f in fields {
                    match t.fields.iter().find(|fm| fm.field == *f && !fm.formula) {
                        Some(fm) if !fm.columns.is_empty() => {
                            cols.extend(fm.columns.iter().map(|c| format!("\"{}\"", c.name)))
                        }
                        _ => return Ok(None),
                    }
                }
                plans.push((ent.clone(), n, fields.clone(), cols, t.name.clone()));
            }
        }
        for (ent, n, fields, cols, table) in &plans {
            let list = cols.join(", ");
            let tuple = if cols.len() == 1 {
                format!("t.{}", cols[0])
            } else {
                format!(
                    "({})",
                    cols.iter()
                        .map(|c| format!("t.{c}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let not_null: Vec<String> = cols.iter().map(|c| format!("{c} IS NOT NULL")).collect();
            let sql = format!(
                "SELECT CAST(t.\"id\" AS TEXT) FROM \"{table}\" t WHERE t.\"id\" > ?bigint AND {tuple} IN (SELECT {list} FROM \"{table}\" WHERE {} GROUP BY {list} HAVING COUNT(*) > 1) ORDER BY t.\"id\" LIMIT 1000",
                not_null.join(" AND ")
            );
            let mut seen: HashMap<String, Id> = HashMap::new();
            let mut last = 0i64;
            'pages: loop {
                let rows = self.conn().conn().query(&sql, &[Sql::Int(last)])?;
                if rows.is_empty() {
                    break;
                }
                let ids: Vec<Id> = rows
                    .iter()
                    .filter_map(|r| r[0].as_deref().and_then(|x| x.parse().ok()))
                    .collect();
                last = ids.last().copied().unwrap_or(0) as i64;
                self.interp.store.prefetch_as(ent, &ids);
                for id in ids {
                    // a later duplicate than one found already: it is not the first
                    if first
                        .as_ref()
                        .is_some_and(|(f, fnr, _)| (*f, *fnr) < (id, *n))
                    {
                        break 'pages;
                    }
                    let rec = V::Ref {
                        ent: Rc::from(ent.as_str()),
                        id,
                    };
                    let mut key = Vec::new();
                    let mut shown = Vec::new();
                    for f in fields {
                        let v = self
                            .interp
                            .get(&rec, f, None, None)
                            .map_err(|e| self.fail(e))?;
                        if v.is_none() {
                            break;
                        }
                        key.push(to_json(&v).to_string());
                        shown.push(self.interp.display(&v));
                    }
                    if key.len() < fields.len() {
                        continue;
                    }
                    if seen.insert(key.join("\u{1}"), id).is_some() {
                        first = Some((
                            id,
                            *n,
                            quadra_runtime::unique_message(ent, fields, false, &shown),
                        ));
                        break 'pages;
                    }
                }
                self.interp.store.evict(&|_| false);
            }
            self.interp.store.evict(&|_| false);
        }
        if let Some((_, _, msg)) = first {
            return Err(flow(NOT_FIT, Flow::Fail(msg)));
        }
        for (ent, p) in passes {
            let n = p.uniques.len();
            steps.push(format!(
                "every {ent}: {n} new unique{} (in SQL)",
                if n == 1 { "" } else { "s" }
            ));
        }
        Ok(Some(()))
    }

    fn passes(
        &mut self,
        passes: &BTreeMap<String, Pass>,
        steps: &mut Vec<String>,
    ) -> Result<(), String> {
        if passes.is_empty() {
            return Ok(());
        }
        for (ent, p) in passes {
            let mut what = Vec::new();
            if !p.defaults.is_empty() {
                what.push(format!("defaults of {}", p.defaults.join(", ")));
            }
            if !p.refill.is_empty() {
                what.push(format!(
                    "{} from the history",
                    p.refill.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            if !p.formulas.is_empty() {
                what.push(format!(
                    "formulas {}",
                    p.formulas.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            let s = &p.sel;
            let rules = if s.all {
                1
            } else {
                s.checks.len() + s.required.len() + s.wheres.len() + s.records.len()
            } + p.uniques.len()
                + p.refreeze as usize;
            if s.all {
                what.push("every rule".into());
            } else if rules > 0 {
                what.push(format!(
                    "{rules} new or changed rule{}",
                    if rules == 1 { "" } else { "s" }
                ));
            }
            if s.warnings {
                what.push("warnings".into());
            }
            steps.push(format!("every {ent}: {}", what.join(", ")));
        }
        let mut cursors = Cursors::new(passes.keys().cloned(), self.tx - 1);
        loop {
            let n = BATCH.load(std::sync::atomic::Ordering::Relaxed).max(1);
            let batch = { cursors.next(self.conn().conn(), n)? };
            if batch.is_empty() {
                break;
            }
            self.batch(passes, &batch)?;
        }
        Ok(())
    }

    fn batch(
        &mut self,
        passes: &BTreeMap<String, Pass>,
        batch: &[(Id, String)],
    ) -> Result<(), String> {
        let m = self.m;
        let tx = self.tx;
        let ids: Vec<Id> = batch.iter().map(|b| b.0).collect();
        let owned: HashSet<Id> = ids.iter().copied().collect();
        {
            let s = &mut self.interp.store;
            s.dirty.clear();
            s.thawed.clear();
            s.posted.clear();
            s.posted_rows.clear();
            s.fired.clear();
            s.prefetch(&ids);
            for id in &ids {
                if !self.overlay.contains(id) {
                    self.new_defaults += s.records.get(id).map_or(0, |r| r.pending_defaults.len());
                }
            }
        }
        if let Some(e) = self.interp.store.source_error() {
            return Err(format!("database: {e}"));
        }
        let i = &mut self.interp;
        let r = i
            .flush_defaults()
            .and_then(|_| i.settle())
            .and_then(|_| i.thaw_records())
            .and_then(|_| i.check_provisional());
        r.map_err(|e| self.fail(e))?;
        // `frozen when` changed
        let mut frozen_changed: BTreeSet<Id> = BTreeSet::new();
        for (id, ent) in batch {
            if passes[ent].refreeze && self.interp.refreeze(*id).map_err(|e| self.fail(e))? {
                frozen_changed.insert(*id);
            }
        }
        // rules: those reading what was written, and those the deploy adds or changes
        let mut warned: HashMap<Id, Vec<String>> = HashMap::new();
        let mut checked: HashSet<Id> = HashSet::new();
        let mut missing = Vec::new();
        for e in concrete(m).filter(|e| !e.is_ledger) {
            let deps = self.interp.record_checks_deps(e);
            let mut ids = match self.interp.candidates_strict(&e.name, &deps) {
                Some(ids) => ids,
                None => {
                    self.later.entry(e.name.clone()).or_default().sel.all = true;
                    Vec::new()
                }
            };
            ids.extend(self.interp.thawed_of(&e.name));
            ids.sort();
            ids.dedup();
            for id in ids {
                if self
                    .interp
                    .store
                    .records
                    .get(&id)
                    .map_or(true, |r| *r.ent != *e.name)
                {
                    continue;
                }
                let before = self
                    .interp
                    .store
                    .records
                    .get(&id)
                    .map(|r| r.warnings.clone())
                    .unwrap_or_default();
                self.interp
                    .check_record_sel(e, id, None, &mut missing)
                    .map_err(|err| self.fail(err))?;
                warned.insert(id, before);
                checked.insert(id);
            }
        }
        for (id, ent) in batch {
            let p = &passes[ent];
            if checked.contains(id) || p.sel.is_empty() {
                continue;
            }
            let e = m.entity(ent).unwrap();
            let before = self
                .interp
                .store
                .records
                .get(id)
                .map(|r| r.warnings.clone())
                .unwrap_or_default();
            self.interp
                .check_record_sel(e, *id, Some(&p.sel), &mut missing)
                .map_err(|err| self.fail(err))?;
            warned.insert(*id, before);
        }
        for (id, msg) in missing {
            let at = self.missing.entry(msg).or_insert(id);
            *at = (*at).min(id);
        }
        // uniqueness: of what was written, and the new rules over every record
        let written_ents: BTreeSet<String> = self
            .interp
            .store
            .dirty
            .iter()
            .filter_map(|id| self.interp.store.records.get(id).map(|r| r.ent.to_string()))
            .collect();
        for ent in &written_ents {
            if let Some(e) = m.entity(ent) {
                self.interp
                    .check_uniques_written(e)
                    .map_err(|err| self.fail(err))?;
            }
        }
        for (id, ent) in batch {
            let p = &passes[ent];
            for (fields, when) in &p.uniques {
                let rec = V::Ref {
                    ent: Rc::from(ent.as_str()),
                    id: *id,
                };
                if let Some(w) = when {
                    let mut ctx = quadra_runtime::Ctx {
                        this: Some(rec.clone()),
                        entity: Some(ent.clone()),
                        scopes: vec![Default::default()],
                        company: None,
                        as_of: None,
                    };
                    if !self
                        .interp
                        .eval(w, &mut ctx)
                        .map_err(|e| self.fail(e))?
                        .truthy()
                    {
                        continue;
                    }
                }
                let mut key = Vec::new();
                let mut shown = Vec::new();
                for f in fields {
                    let v = self
                        .interp
                        .get(&rec, f, None, None)
                        .map_err(|e| self.fail(e))?;
                    if v.is_none() {
                        break;
                    }
                    key.push(to_json(&v).to_string());
                    shown.push(self.interp.display(&v));
                }
                if key.len() < fields.len() {
                    continue;
                }
                let seen = self
                    .uniques
                    .entry(format!("{ent}|{}|{}", fields.join(","), when.is_some()))
                    .or_default();
                if seen.insert(key.join("\u{1}"), *id).is_some() {
                    return Err(flow(
                        NOT_FIT,
                        Flow::Fail(quadra_runtime::unique_message(
                            ent,
                            fields,
                            when.is_some(),
                            &shown,
                        )),
                    ));
                }
            }
        }
        self.interp.freeze().map_err(|e| self.fail(e))?;
        // formulas: those reading what was written, and those whose definition changed
        let mut formulas: BTreeMap<Id, BTreeSet<String>> = BTreeMap::new();
        let targets = self.interp.materialize.clone();
        self.interp.formula_memo = Some(HashMap::new());
        for (ent, field) in &targets {
            let deps = self.interp.formula_deps(ent, field);
            let mut ids = match self.interp.candidates_strict(ent, &deps) {
                Some(ids) => ids,
                None => {
                    self.later
                        .entry(ent.clone())
                        .or_default()
                        .formulas
                        .insert(field.clone());
                    Vec::new()
                }
            };
            ids.extend(self.interp.thawed_of(ent));
            ids.extend(
                batch
                    .iter()
                    .filter(|(_, e)| e == ent && passes[e].formulas.contains(field))
                    .map(|(id, _)| *id),
            );
            ids.sort();
            ids.dedup();
            for id in ids {
                let rec = V::Ref {
                    ent: Rc::from(ent.as_str()),
                    id,
                };
                let v = self.interp.get(&rec, field, None, None).unwrap_or(V::None);
                self.interp
                    .store
                    .materialized
                    .insert((id, field.clone()), v);
                formulas.entry(id).or_default().insert(field.clone());
            }
        }
        self.interp.formula_memo = None;
        if let Some(e) = self.interp.store.source_error() {
            return Err(format!("database: {e}"));
        }
        // what changed, to the tables
        let mut touched: BTreeSet<Id> = self.interp.store.dirty.iter().copied().collect();
        touched.extend(frozen_changed.iter().copied());
        touched.extend(self.interp.store.thawed.iter().copied());
        touched.extend(formulas.keys().copied());
        touched.extend(
            warned
                .iter()
                .filter(|(id, w)| {
                    self.interp
                        .store
                        .records
                        .get(id)
                        .is_some_and(|r| r.warnings != **w)
                })
                .map(|(id, _)| *id),
        );
        for (id, ent) in batch {
            if !passes[ent].refill.is_empty() {
                touched.insert(*id);
            }
        }
        for id in touched {
            if self.overlay.contains(&id) {
                continue;
            }
            let Some(r) = self.interp.store.records.get(&id) else {
                continue;
            };
            let entries: Vec<(String, V)> = r
                .history
                .iter()
                .filter(|(t, _, _)| *t == tx)
                .map(|(_, f, v)| (f.clone(), v.clone()))
                .collect();
            if r.created_tx == tx {
                self.overlay.insert(id);
                continue;
            }
            let ent = r.ent.to_string();
            if !owned.contains(&id) && !entries.is_empty() {
                // a record of another batch: its own batch gives it its defaults
                let defaults = self.plan.defaults.get(&ent);
                if entries
                    .iter()
                    .all(|(f, _)| defaults.is_some_and(|d| d.contains(f)))
                    && self
                        .plan
                        .passes
                        .get(&ent)
                        .is_some_and(|p| !p.defaults.is_empty())
                {
                    continue;
                }
                if self.emitted.contains(&id) {
                    return Err("a record was written by two batches of the deploy".into());
                }
                self.overlay.insert(id);
                continue;
            }
            if !entries.is_empty() && !self.emitted.insert(id) {
                return Err("a record was written by two batches of the deploy".into());
            }
            let frozen = frozen_changed.contains(&id)
                || self.interp.store.thawed.contains(&id)
                || r.frozen_at == Some(tx);
            let warnings_changed = warned.get(&id).is_some_and(|w| *w != r.warnings);
            for (f, v) in &entries {
                self.ord += 1;
                self.hist.push(vec![
                    Sql::Int(id as i64),
                    Sql::Int(self.ord),
                    Sql::Text(f.clone()),
                    Sql::Text(to_json(v).to_string()),
                ]);
            }
            if !entries.is_empty() || frozen || warnings_changed {
                let frozen_tx = r.frozen_at.map(|t| Sql::Int(t as i64)).unwrap_or(Sql::Null);
                let kinds = crate::db::kinds_of(&self.schema, r)
                    .map(Sql::Text)
                    .unwrap_or(Sql::Null);
                self.stager.add(
                    "_quadra_records",
                    vec![
                        "last_tx".into(),
                        "version_tx".into(),
                        "frozen_tx".into(),
                        "warnings".into(),
                        "kinds".into(),
                    ],
                    vec![
                        Sql::Int(id as i64),
                        Sql::Int(tx as i64),
                        Sql::Int(if crate::db::exact_row(&self.schema, r) {
                            r.version() as i64
                        } else {
                            0
                        }),
                        frozen_tx.clone(),
                        Sql::Text(serde_json::to_string(&r.warnings).unwrap()),
                        kinds,
                    ],
                );
            } else if owned.contains(&id) && passes.get(&ent).is_some_and(|p| !p.refill.is_empty())
            {
                // columns filled again (another type): whether the row reads back exactly
                let v = if crate::db::exact_row(&self.schema, r) {
                    r.version() as i64
                } else {
                    0
                };
                let kinds = crate::db::kinds_of(&self.schema, r)
                    .map(Sql::Text)
                    .unwrap_or(Sql::Null);
                self.stager.add(
                    "_quadra_records",
                    vec!["last_tx".into(), "version_tx".into(), "kinds".into()],
                    vec![Sql::Int(id as i64), Sql::Int(tx as i64), Sql::Int(v), kinds],
                );
            } else if formulas.contains_key(&id) {
                // only its materialized formulas: caches and snapshots see the row changed
                self.stager.add(
                    "_quadra_records",
                    vec!["last_tx".into()],
                    vec![Sql::Int(id as i64), Sql::Int(tx as i64)],
                );
            }
            // the projection: the columns of what changed
            let Some(t) = self.schema.entity_table(&ent) else {
                continue;
            };
            let mut fields: BTreeSet<String> = entries
                .iter()
                .map(|(f, _)| f.split(['@', '#']).next().unwrap_or(f).to_string())
                .collect();
            if owned.contains(&id) {
                if let Some(p) = passes.get(&ent) {
                    fields.extend(p.refill.iter().cloned());
                }
            }
            let simple = fields
                .iter()
                .all(|f| t.fields.iter().any(|fm| fm.field == *f && !fm.formula))
                && entries
                    .iter()
                    .all(|(f, _)| !f.contains('@') && !f.contains('#'));
            if !simple {
                self.overlay.insert(id);
                continue;
            }
            let mut cols = Vec::new();
            let mut row = vec![Sql::Int(id as i64)];
            for fm in &t.fields {
                let v = if fm.formula {
                    if !formulas.get(&id).is_some_and(|f| f.contains(&fm.field)) {
                        continue;
                    }
                    self.interp
                        .store
                        .materialized
                        .get(&(id, fm.field.clone()))
                        .cloned()
                        .unwrap_or(V::None)
                } else {
                    if !fields.contains(&fm.field) {
                        continue;
                    }
                    r.fields.get(&fm.field).cloned().unwrap_or(V::None)
                };
                for c in &fm.columns {
                    cols.push(c.name.clone());
                    row.push(encode(c, &v));
                }
            }
            if frozen {
                cols.push("_frozen_tx".into());
                row.push(r.frozen_at.map(|t| Sql::Int(t as i64)).unwrap_or(Sql::Null));
            }
            if !cols.is_empty() {
                self.stager.add(&t.name, cols, row);
            }
        }
        // to the database: history rows of this batch, and the updates
        let hist = std::mem::take(&mut self.hist);
        {
            let mut src = self.src.borrow_mut();
            let conn = src.conn();
            if !hist.is_empty() {
                conn.bulk_insert(
                    "_qx_hist",
                    &["record_id", "ord", "field", "value"],
                    &mut hist.into_iter(),
                )?;
            }
            self.stager.flush(conn)?;
        }
        let overlay = self.overlay.clone();
        self.interp.store.evict(&|id| overlay.contains(&id));
        self.interp.store.dirty.clear();
        self.interp.store.thawed.clear();
        Ok(())
    }

    /// The records kept in memory, to the tables (as a commit writes them).
    fn overlay(&mut self) -> Result<(), String> {
        let tx = self.tx;
        let ids: Vec<Id> = self.overlay.iter().copied().collect();
        for id in &ids {
            let Some(r) = self.interp.store.records.get(id) else {
                continue;
            };
            for (t, f, v) in &r.history {
                if *t == tx {
                    self.ord += 1;
                    self.hist.push(vec![
                        Sql::Int(*id as i64),
                        Sql::Int(self.ord),
                        Sql::Text(f.clone()),
                        Sql::Text(to_json(v).to_string()),
                    ]);
                }
            }
        }
        let hist = std::mem::take(&mut self.hist);
        if !hist.is_empty() {
            self.conn().conn().bulk_insert(
                "_qx_hist",
                &["record_id", "ord", "field", "value"],
                &mut hist.into_iter(),
            )?;
        }
        let m = self.m;
        let store = std::mem::take(&mut self.interp.store);
        let r = self.with_db(|db| {
            for id in &ids {
                let Some(r) = store.records.get(id) else {
                    continue;
                };
                let changed = r.created_tx == tx
                    || r.history.iter().any(|(t, _, _)| *t == tx)
                    || r.frozen_at == Some(tx)
                    || store.thawed.contains(id);
                if changed || store.materialized.keys().any(|(x, _)| x == id) {
                    db.record_meta(r, tx, r.version())?;
                }
                db.project_record(m, &store, *id, &|t| t)?;
            }
            for row in store.rows.iter().filter(|r| r.tx == tx) {
                db.project_row(row)?;
            }
            Ok(())
        });
        self.interp.store = store;
        r
    }

    /// The history entries collected, in record order, to `_quadra_history`; the commit row.
    fn commit(&mut self, cause: &str, time: NaiveDateTime) -> Result<(), String> {
        let tx = self.tx;
        let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
        let (mut seq, prev) = {
            let mut src = self.src.borrow_mut();
            let conn = src.conn();
            let seq = conn.query(
                "SELECT CAST(MAX(\"seq\") AS TEXT) FROM \"_quadra_history\"",
                &[],
            )?;
            let seq: i64 = seq
                .first()
                .and_then(|r| r[0].as_deref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let last = conn.query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1", &[])?;
            (
                seq,
                last.first()
                    .and_then(|r| r[0].clone())
                    .unwrap_or_else(|| "0".repeat(64)),
            )
        };
        self.src
            .borrow_mut()
            .conn()
            .batch("CREATE INDEX \"_qx_hist_order\" ON \"_qx_hist\" (\"record_id\", \"ord\")")?;
        let mut hasher = CommitHasher::new(&prev, tx, &at, None, cause);
        let (mut last_id, mut last_ord) = (-1i64, -1i64);
        loop {
            let mut src = self.src.borrow_mut();
            let conn = src.conn();
            let rows = conn.query(
                "SELECT CAST(\"record_id\" AS TEXT), CAST(\"ord\" AS TEXT), \"field\", \"value\" FROM \"_qx_hist\" WHERE (\"_qx_hist\".\"record_id\", \"_qx_hist\".\"ord\") > (?bigint, ?bigint) ORDER BY \"_qx_hist\".\"record_id\", \"_qx_hist\".\"ord\" LIMIT 10000",
                &[Sql::Int(last_id), Sql::Int(last_ord)],
            )?;
            if rows.is_empty() {
                break;
            }
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let id: i64 = r[0].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                last_ord = r[1].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                last_id = id;
                let field = r[2].clone().unwrap_or_default();
                let value = r[3].clone().unwrap_or_default();
                hasher.entry(&json!(["h", id, field, value]).to_string());
                seq += 1;
                out.push(vec![
                    Sql::Int(seq),
                    Sql::Int(tx as i64),
                    Sql::Int(id),
                    Sql::Text(field),
                    Sql::Text(value),
                ]);
            }
            conn.bulk_insert(
                "_quadra_history",
                &["seq", "tx", "record_id", "field", "value"],
                &mut out.into_iter(),
            )?;
        }
        // applied migrations and renames: in the hash after the history, as `write_tx` does
        let store = std::mem::take(&mut self.interp.store);
        let empty = Store::default();
        let commit = Commit {
            tx,
            base: tx - 1,
            before: &empty,
            after: &store,
            user: None,
            cause: cause.to_string(),
            time,
        };
        let mut payload = Vec::new();
        let r = self.with_db(|db| {
            db.write_scripts(&commit, &mut payload, &|t| t)?;
            db.write_outbox(&commit, tx, None)?;
            Ok(())
        });
        let next_id = store.next_id;
        self.interp.store = store;
        r?;
        for p in &payload {
            hasher.entry(&p.to_string());
        }
        let hash = hasher.finish();
        let mut src = self.src.borrow_mut();
        let conn = src.conn();
        let get = |conn: &mut Conn, k: &str| -> Result<Option<String>, String> {
            Ok(conn
                .query(
                    "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = ?text",
                    &[Sql::Text(k.into())],
                )?
                .into_iter()
                .next()
                .and_then(|r| r[0].clone()))
        };
        let stored_next = get(conn, "state")?
            .and_then(|s| serde_json::from_str::<J>(&s).ok())
            .and_then(|s| s["next_id"].as_u64())
            .unwrap_or(0);
        let reserved = get(conn, "id_reserved")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let state =
            json!({ "next_id": stored_next.max(reserved).max(next_id), "tx": tx }).to_string();
        conn.exec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('state', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"", &[Sql::Text(state)])?;
        conn.exec(
            "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text)",
            &[Sql::Int(tx as i64), Sql::Text(at), Sql::Null, Sql::Text(cause.to_string()), Sql::Text(prev), Sql::Text(hash)],
        )?;
        Ok(())
    }
}

/// The definition of a table without the `NOT NULL` of `relax`, the `CHECK`s and the
/// `UNIQUE`s the old table did not have: what the data is checked against first (the engine
/// reports what breaks the new ones). Whether anything was left out.
fn permissive(t: &Table, relax: &BTreeSet<String>, old: &Table) -> (Table, bool) {
    let mut t = t.clone();
    let mut relaxed = false;
    for c in t.columns.iter_mut() {
        if relax.contains(&c.name) {
            c.not_null = false;
            relaxed = true;
        }
        let before = old.column(&c.name).and_then(|oc| oc.check.clone());
        if c.check.is_some() && c.check != before {
            c.check = before;
            relaxed = true;
        }
    }
    let before = t.uniques.len();
    t.uniques.retain(|u| old.uniques.contains(u));
    relaxed |= t.uniques.len() != before;
    (t, relaxed)
}

/// SQLite: replace a table by a copy with another definition (the columns of both are copied).
fn sqlite_rebuild(conn: &mut Conn, def: &Table) -> Result<(), String> {
    let tmp = format!("{}__qx_new", def.name);
    let existing: BTreeSet<String> = conn
        .query(
            &format!(
                "SELECT \"name\" FROM pragma_table_info('{}')",
                def.name.replace('\'', "''")
            ),
            &[],
        )?
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect();
    let cols: Vec<String> = def
        .columns
        .iter()
        .filter(|c| existing.contains(&c.name))
        .map(|c| format!("\"{}\"", c.name))
        .collect();
    conn.batch(&format!("DROP TABLE IF EXISTS \"{tmp}\""))?;
    conn.batch(&def.create_sql_as(&tmp, Dialect::Sqlite))?;
    conn.batch(&format!(
        "INSERT INTO \"{tmp}\" ({0}) SELECT {0} FROM \"{1}\"",
        cols.join(", "),
        def.name
    ))?;
    conn.batch(&format!("DROP TABLE \"{}\"", def.name))?;
    conn.batch(&format!("ALTER TABLE \"{tmp}\" RENAME TO \"{}\"", def.name))?;
    Ok(())
}

/// PostgreSQL: the constraints of a table of type `kind` (`u`, `c`, `f`), with their columns.
pub(crate) fn pg_constraints(
    conn: &mut Conn,
    table: &str,
    kind: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let rows = conn.query(
        "SELECT CAST(c.conname AS TEXT), CAST(a.attname AS TEXT) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
         CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
         WHERE n.nspname = current_schema() AND t.relname = ?text AND c.contype = ?text ORDER BY c.conname, k.ord",
        &[Sql::Text(table.into()), Sql::Text(kind.into())],
    )?;
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let (name, col) = (
            r[0].clone().unwrap_or_default(),
            r[1].clone().unwrap_or_default(),
        );
        match out.last_mut() {
            Some((n, cols)) if *n == name => cols.push(col),
            _ => out.push((name, vec![col])),
        }
    }
    Ok(out)
}

/// The rows of a ledger, as `project_row` writes them, to a new ledger table.
fn fill_ledger(conn: &mut Conn, t: &Table) -> Result<(), String> {
    let TableSource::Ledger(ledger) = &t.source else {
        return Ok(());
    };
    let mut names: Vec<String> = ["id", "_tx", "_date", "_source_entity", "_source_id"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for fm in &t.fields {
        names.extend(fm.columns.iter().map(|c| c.name.clone()));
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut last = 0i64;
    loop {
        let rows = conn.query(
            &format!(
                "SELECT {} FROM \"_quadra_ledger_rows\" WHERE \"ledger\" = ?text AND \"id\" > ?bigint ORDER BY \"_quadra_ledger_rows\".\"ledger\", \"_quadra_ledger_rows\".\"id\" LIMIT 5000",
                crate::source::ROW_COLS
            ),
            &[Sql::Text(ledger.clone()), Sql::Int(last)],
        )?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut out = Vec::new();
        for r in &rows {
            let row = crate::source::decode_row(r);
            last = row.id as i64;
            let (src_ent, src_id) = match &row.source {
                Some(V::Ref { ent, id }) => (Sql::Text(ent.to_string()), Sql::Int(*id as i64)),
                _ => (Sql::Null, Sql::Null),
            };
            let mut v = vec![
                Sql::Int(row.id as i64),
                Sql::Int(row.tx as i64),
                Sql::Text(row.date.format("%Y-%m-%d %H:%M:%S").to_string()),
                src_ent,
                src_id,
            ];
            for fm in &t.fields {
                let val = row.cols.get(&fm.field).cloned().unwrap_or(V::None);
                for c in &fm.columns {
                    v.push(encode(c, &val));
                }
            }
            out.push(v);
        }
        conn.bulk_insert(&t.name, &refs, &mut out.into_iter())?;
    }
}

/// The changes of the tables, before the data: renames, new tables and columns, drops.
/// Returns the tables created or rebuilt (their indexes are built in the transaction) and
/// the SQLite tables to copy at the end (to their exact definition).
fn ddl_before(
    conn: &mut Conn,
    plan: &Plan,
    renamed_tables: &[(String, String)],
    col_renames: &[(String, String, String)],
    old: &Catalog,
    steps: &mut Vec<String>,
) -> Result<(BTreeSet<String>, Vec<Table>), String> {
    let d = conn.dialect();
    let mut fresh = BTreeSet::new();
    let mut end = Vec::new();
    for (from, to) in renamed_tables {
        conn.batch(&format!("ALTER TABLE \"{from}\" RENAME TO \"{to}\""))?;
        steps.push(format!("rename table {from} → {to}"));
    }
    for (t, from, to) in col_renames {
        conn.batch(&format!(
            "ALTER TABLE \"{t}\" RENAME COLUMN \"{from}\" TO \"{to}\""
        ))?;
        steps.push(format!("rename column {t}.{from} → {to}"));
    }
    for op in &plan.tables {
        match op {
            TableOp::Create(t) => {
                conn.batch(&t.create_sql(d))?;
                fresh.insert(t.name.clone());
                steps.push(format!("create table {}", t.name));
            }
            TableOp::Alter(a) => {
                let t = &a.table;
                let dropping: BTreeSet<&String> = a.drop.iter().collect();
                // the `qx_` indexes on dropped columns go first (SQLite refuses to drop them), and
                // the triggers of the full-text search (they name the columns; the search is built
                // again at the end)
                for i in old
                    .indexes
                    .iter()
                    .filter(|i| i.table == t.name && i.columns.iter().any(|c| dropping.contains(c)))
                {
                    conn.batch(&format!("DROP INDEX IF EXISTS \"{}\"", i.name))?;
                }
                if d == Dialect::Sqlite {
                    crate::db::drop_search_triggers(conn, &t.name)?;
                }
                let droppable = |c: &str| {
                    a.old.column(c).is_some_and(|oc| oc.references.is_none())
                        && !a.old.uniques.iter().any(|u| u.iter().any(|x| x == c))
                        && !a.old.primary.iter().any(|p| p == c)
                };
                if d == Dialect::Sqlite && (a.constraints || !a.drop.iter().all(|c| droppable(c))) {
                    // the table is copied: now, without the constraints the data must be checked
                    // against first (the engine reports what breaks them), and at the end with them
                    let mut relax: BTreeSet<String> = a
                        .add
                        .iter()
                        .filter(|c| c.not_null)
                        .map(|c| c.name.clone())
                        .collect();
                    relax.extend(
                        t.columns
                            .iter()
                            .filter(|c| {
                                c.not_null && a.old.column(&c.name).is_some_and(|oc| !oc.not_null)
                            })
                            .map(|c| c.name.clone()),
                    );
                    let (def, relaxed) = permissive(t, &relax, &a.old);
                    sqlite_rebuild(conn, &def)?;
                    fresh.insert(t.name.clone());
                    if relaxed {
                        end.push(t.clone());
                    }
                    steps.push(format!("copy table {}", t.name));
                    continue;
                }
                for c in &a.drop {
                    conn.batch(&format!("ALTER TABLE \"{}\" DROP COLUMN \"{c}\"", t.name))?;
                    steps.push(format!("drop column {}.{c}", t.name));
                }
                for c in &a.add {
                    let mut def = c.definition(d, false);
                    if let (Dialect::Sqlite, Some(r)) = (d, &c.references) {
                        def.push_str(&format!(
                            " REFERENCES \"{r}\" (\"id\") DEFERRABLE INITIALLY DEFERRED"
                        ));
                    }
                    conn.batch(&format!("ALTER TABLE \"{}\" ADD COLUMN {def}", t.name))?;
                    steps.push(format!("add column {}.{}", t.name, c.name));
                }
                if d == Dialect::Sqlite && a.add.iter().any(|c| c.not_null) {
                    end.push(t.clone());
                    steps.push(format!("copy table {} (its new NOT NULL columns)", t.name));
                }
            }
            TableOp::Ledger(t) => {
                let cascade = if d == Dialect::Postgres {
                    " CASCADE"
                } else {
                    ""
                };
                conn.batch(&format!("DROP TABLE IF EXISTS \"{}\"{cascade}", t.name))?;
                conn.batch(&t.create_sql(d))?;
                fill_ledger(conn, t)?;
                fresh.insert(t.name.clone());
                steps.push(format!("rebuild ledger table {}", t.name));
            }
            TableOp::Trait(t) => {
                conn.batch(&format!("DELETE FROM \"{}\"", t.name))?;
                fresh.insert(t.name.clone());
            }
            TableOp::Drop(_) => {}
        }
    }
    for op in &plan.tables {
        if let TableOp::Drop(name) = op {
            let cascade = if d == Dialect::Postgres {
                " CASCADE"
            } else {
                ""
            };
            conn.batch(&format!("DROP TABLE IF EXISTS \"{name}\"{cascade}"))?;
            steps.push(format!("drop table {name}"));
        }
    }
    Ok((fresh, end))
}

/// Trait tables created or whose implementors changed: filled from the records.
fn fill_traits(conn: &mut Conn, plan: &Plan, new: &Catalog) -> Result<(), String> {
    for op in &plan.tables {
        let (TableOp::Create(t) | TableOp::Trait(t)) = op else {
            continue;
        };
        if !matches!(t.source, TableSource::Trait(_)) {
            continue;
        }
        let ents = new.traits.get(&t.name).cloned().unwrap_or_default();
        if ents.is_empty() {
            continue;
        }
        let list: Vec<String> = ents
            .iter()
            .map(|e| format!("'{}'", e.replace('\'', "''")))
            .collect();
        conn.batch(&format!("INSERT INTO \"{}\" (\"id\", \"entity\") SELECT \"id\", \"entity\" FROM \"_quadra_records\" WHERE \"entity\" IN ({})", t.name, list.join(", ")))?;
    }
    Ok(())
}

/// The changes of the tables after the data: `NOT NULL`, constraints and foreign keys
/// (PostgreSQL), the SQLite copies to the exact definition.
fn ddl_after(
    conn: &mut Conn,
    plan: &Plan,
    end: &[Table],
    copied: &BTreeSet<String>,
) -> Result<(), String> {
    match conn.dialect() {
        Dialect::Sqlite => {
            for t in end {
                sqlite_rebuild(conn, t)?;
            }
            // the tables copied with the foreign keys off: their references still hold
            for t in copied {
                let bad = conn.query(
                    &format!(
                        "SELECT \"table\" FROM pragma_foreign_key_check('{}')",
                        t.replace('\'', "''")
                    ),
                    &[],
                )?;
                if let Some(r) = bad.first() {
                    return Err(format!(
                        "a reference of {} points to no record",
                        r[0].clone().unwrap_or_default()
                    ));
                }
            }
        }
        Dialect::Postgres => {
            // the deferred foreign keys checked now: a table with pending checks cannot be altered
            conn.batch("SET CONSTRAINTS ALL IMMEDIATE")?;
            for op in &plan.tables {
                match op {
                    TableOp::Create(t) | TableOp::Ledger(t) => {
                        for fk in t.foreign_keys() {
                            conn.batch(&fk)?;
                        }
                    }
                    TableOp::Alter(a) => {
                        let t = &a.table;
                        for c in &a.add {
                            if c.not_null {
                                conn.batch(&format!(
                                    "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" SET NOT NULL",
                                    t.name, c.name
                                ))?;
                            }
                            if let Some(r) = &c.references {
                                conn.batch(&schema::foreign_key(&t.name, &c.name, r))?;
                            }
                        }
                        if !a.constraints {
                            continue;
                        }
                        let added: BTreeSet<&String> = a.add.iter().map(|c| &c.name).collect();
                        for c in t.columns.iter().filter(|c| !added.contains(&c.name)) {
                            let Some(oc) = a.old.column(&c.name) else {
                                continue;
                            };
                            if oc.not_null != c.not_null && !t.primary.contains(&c.name) {
                                let set = if c.not_null { "SET" } else { "DROP" };
                                conn.batch(&format!(
                                    "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" {set} NOT NULL",
                                    t.name, c.name
                                ))?;
                            }
                            if oc.check != c.check {
                                for (name, cols) in pg_constraints(conn, &t.name, "c")? {
                                    if cols == [c.name.clone()] {
                                        conn.batch(&format!(
                                            "ALTER TABLE \"{}\" DROP CONSTRAINT \"{name}\"",
                                            t.name
                                        ))?;
                                    }
                                }
                                if let Some(ch) = &c.check {
                                    conn.batch(&format!(
                                        "ALTER TABLE \"{}\" ADD CHECK ({ch})",
                                        t.name
                                    ))?;
                                }
                            }
                            if oc.references != c.references {
                                for (name, cols) in pg_constraints(conn, &t.name, "f")? {
                                    if cols == [c.name.clone()] {
                                        conn.batch(&format!(
                                            "ALTER TABLE \"{}\" DROP CONSTRAINT \"{name}\"",
                                            t.name
                                        ))?;
                                    }
                                }
                                if let Some(r) = &c.references {
                                    conn.batch(&schema::foreign_key(&t.name, &c.name, r))?;
                                }
                            }
                        }
                        if a.old.uniques != t.uniques {
                            for (name, cols) in pg_constraints(conn, &t.name, "u")? {
                                if !t.uniques.contains(&cols) {
                                    conn.batch(&format!(
                                        "ALTER TABLE \"{}\" DROP CONSTRAINT \"{name}\"",
                                        t.name
                                    ))?;
                                }
                            }
                            let now: Vec<Vec<String>> = pg_constraints(conn, &t.name, "u")?
                                .into_iter()
                                .map(|(_, c)| c)
                                .collect();
                            for u in t.uniques.iter().filter(|u| !now.contains(u)) {
                                conn.batch(&format!(
                                    "ALTER TABLE \"{}\" ADD UNIQUE ({})",
                                    t.name,
                                    u.iter()
                                        .map(|c| format!("\"{c}\""))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ))?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Indexes renamed with their table or column (PostgreSQL: `ALTER INDEX … RENAME`; SQLite
/// builds them again with the others).
fn rename_indexes(
    conn: &mut Conn,
    old: &Catalog,
    schema: &Schema,
    existing: &[(String, String, bool)],
) -> Result<(), String> {
    if conn.dialect() != Dialect::Postgres {
        return Ok(());
    }
    for i in &old.indexes {
        let Some(want) = schema.indexes.iter().find(|w| {
            w.table == i.table && w.columns == i.columns && w.lower == i.lower && w.fts == i.fts
        }) else {
            continue;
        };
        if want.name != i.name
            && existing.iter().any(|(n, _, valid)| *n == i.name && *valid)
            && !existing.iter().any(|(n, _, _)| *n == want.name)
        {
            conn.batch(&format!(
                "ALTER INDEX \"{}\" RENAME TO \"{}\"",
                i.name, want.name
            ))?;
        }
    }
    Ok(())
}

/// The outcome of an incremental deploy.
pub(crate) enum Done {
    /// nothing to do (`migrate` with the schema current and no pending migration)
    Nothing,
    Report(MigrateReport),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn deploy(
    url: &str,
    model: &Model,
    report: &Report,
    root: &Path,
    time: NaiveDateTime,
    dry: bool,
    always: bool,
    label: Option<&str>,
) -> Result<Result<Done, Fallback>, String> {
    let mut db = Database::open(url, model, report)?;
    let status = db.status()?;
    if !status.initialized {
        return Err("the database is not initialized (run `quadra db init`)".into());
    }
    let Some(mut old) = Catalog::read(&mut db)? else {
        return Ok(Err(Fallback("the database has no catalog yet".into())));
    };
    let applied: BTreeSet<String> = db
        .conn
        .query("SELECT \"key\" FROM \"_quadra_applied\"", &[])?
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect();
    let pending: Vec<&Block> = model
        .migrations
        .iter()
        .filter(|m| !applied.contains(&format!("migration:{}", m.name)))
        .collect();
    if status.schema_current && pending.is_empty() && !always {
        return Ok(Ok(Done::Nothing));
    }
    let (added, removed) = db.plan()?;
    let mut out = MigrateReport {
        schema_changed: !status.schema_current,
        fingerprint: status.fingerprint.clone(),
        added,
        removed,
        incremental: true,
        ..Default::default()
    };
    let renames = pending_renames(model, &pending);
    let (col_renames, renamed_tables) = rename_catalog(&mut old, &renames);
    let schema = db.schema.clone();
    let new = Catalog::of(model, &schema, Some(&old));
    let mut plan = match make_plan(model, &schema, &old, &new) {
        Ok(p) => p,
        Err(f) => return Ok(Err(f)),
    };
    let tables = Plan {
        tables: std::mem::take(&mut plan.tables),
        passes: BTreeMap::new(),
        defaults: HashMap::new(),
    };
    let names: Vec<String> = pending.iter().map(|m| m.name.clone()).collect();
    let cause = match (label, names.is_empty()) {
        (Some(l), true) => l.to_string(),
        (Some(l), false) => format!("{l} (migrations: {})", names.join(", ")),
        (None, true) => "schema migration".to_string(),
        (None, false) => format!("schema migration: {}", names.join(", ")),
    };
    let d = db.dialect();
    // SQLite: a table copied while other tables refer to it needs foreign keys off (they are
    // checked before the commit)
    let copies = d == Dialect::Sqlite
        && tables
            .tables
            .iter()
            .any(|op| !matches!(op, TableOp::Create(_) | TableOp::Trait(_)));
    if copies {
        db.conn.batch("PRAGMA foreign_keys = OFF")?;
    }
    db.conn.batch(if d == Dialect::Sqlite {
        "BEGIN IMMEDIATE"
    } else {
        "BEGIN"
    })?;
    let mut steps = Vec::new();
    let mut concurrent: BTreeSet<String> = BTreeSet::new();
    let result = (|| -> Result<(), String> {
        if d == Dialect::Postgres {
            db.conn
                .batch("LOCK TABLE \"_quadra_commits\" IN EXCLUSIVE MODE")?;
        }
        let base = head(&mut db.conn)?;
        let existing = db.existing_indexes()?;
        let (fresh, end) = ddl_before(
            &mut db.conn,
            &tables,
            &renamed_tables,
            &col_renames,
            &old,
            &mut steps,
        )?;
        fill_traits(&mut db.conn, &tables, &new)?;
        // a field whose columns now read back as another kind of value, an entity without
        // its table: its records are read from the history until written again
        // (`_quadra_records.version_tx`)
        for o in &old.tables {
            if let TableSource::Entity(ent) = &o.source {
                if schema.entity_table(ent).is_none() {
                    db.conn.exec("UPDATE \"_quadra_records\" SET \"version_tx\" = 0 WHERE \"entity\" = ?text AND \"version_tx\" <> 0", &[Sql::Text(ent.clone())])?;
                }
            }
        }
        for t in &schema.tables {
            let TableSource::Entity(ent) = &t.source else {
                continue;
            };
            let Some(o) = old.tables.iter().find(|x| x.name == t.name) else {
                continue;
            };
            let changed = t.fields.iter().filter(|fm| !fm.formula).any(|fm| {
                o.fields
                    .iter()
                    .find(|x| x.field == fm.field && !x.formula)
                    .is_some_and(|x| x.decode != fm.decode && x.decode != schema::Decode::Unknown)
            });
            if changed {
                db.conn.exec("UPDATE \"_quadra_records\" SET \"version_tx\" = 0 WHERE \"entity\" = ?text AND \"version_tx\" <> 0", &[Sql::Text(ent.clone())])?;
            }
        }
        // the engine, reading through the deploy's connection (inside its transaction)
        let spare = Pooled::own(Conn::open(url)?);
        let renames_now = crate::source::load_renames(&mut db.conn)?;
        let src = Rc::new(RefCell::new(DbSource::with_renames(
            spare,
            base,
            schema.clone(),
            None,
            renames_now,
        )?));
        src.borrow_mut().swap_conn(&mut db.conn);
        src.borrow_mut().in_tx = true;
        let shared: SharedSource = src.clone();
        let mut interp = Interp::new(model);
        interp.set_clock(time);
        interp.store = Store::over(shared);
        interp.store.untracked = true;
        interp.load_units();
        interp.system += 1;
        interp.whole_data = true;
        interp.materialize = materialized_formulas(&schema);
        let tx = base + 1;
        interp.store.tx = tx;
        interp.store.commits.insert(tx, (time, None));
        let parent = interp.cause.clone();
        interp.cause = Rc::new(Cause {
            desc: cause.clone(),
            user: "system".into(),
            time,
            parent: Some(parent),
        });
        let mut run = Run {
            m: model,
            schema: schema.clone(),
            db,
            src,
            interp,
            tx,
            plan,
            overlay: BTreeSet::new(),
            emitted: HashSet::new(),
            stager: Stager::default(),
            hist: Vec::new(),
            ord: 0,
            missing: BTreeMap::new(),
            uniques: HashMap::new(),
            later: BTreeMap::new(),
            new_defaults: 0,
        };
        let r = (|| -> Result<(), String> {
            run.conn().conn().batch("CREATE TEMP TABLE \"_qx_hist\" (\"record_id\" BIGINT NOT NULL, \"ord\" BIGINT NOT NULL, \"field\" TEXT NOT NULL, \"value\" TEXT NOT NULL)")?;
            run.conn()
                .conn()
                .batch("DROP INDEX IF EXISTS \"_qx_hist_order\"")?;
            if !pending.is_empty() {
                let mut scripts = Vec::new();
                run.migrations(&pending, root, &mut scripts)?;
                out.scripts = scripts;
            } else {
                run.interp.store.load_defaults = run.plan.defaults.clone();
            }
            let passes = run.plan.passes.clone();
            let in_sql = if pending.is_empty() {
                run.sql_checks(&passes, &mut steps)?
            } else {
                None
            };
            if in_sql.is_none() {
                run.passes(&passes, &mut steps)?;
            }
            // rules and formulas that read a whole entity the batches wrote: once, everywhere
            let later = std::mem::take(&mut run.later);
            if !later.is_empty() {
                let mut again = BTreeMap::new();
                for (ent, mut p) in later {
                    if p.sel.all {
                        p.sel = RuleSel {
                            all: true,
                            ..Default::default()
                        };
                    }
                    if run.schema.entity_table(&ent).is_some() {
                        again.insert(ent, p);
                    }
                }
                run.passes(&again, &mut steps)?;
            }
            if !run.interp.violations.is_empty() {
                return Err(run.interp.violations_report());
            }
            if !run.missing.is_empty() {
                let mut msgs: Vec<(Id, String)> =
                    run.missing.iter().map(|(m, id)| (*id, m.clone())).collect();
                msgs.sort();
                return Err(flow(
                    NOT_FIT,
                    Flow::Fail(
                        msgs.into_iter()
                            .take(6)
                            .map(|(_, m)| m)
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                ));
            }
            run.overlay()?;
            run.commit(&cause, time)?;
            {
                let mut src = run.src.borrow_mut();
                let conn = src.conn();
                run.stager.drop_all(conn)?;
                conn.batch("DROP TABLE IF EXISTS \"_qx_hist\"")?;
            }
            Ok(())
        })();
        out.new_defaults = run.new_defaults;
        let Run {
            mut db,
            src,
            interp,
            ..
        } = run;
        drop(interp);
        src.borrow_mut().swap_conn(&mut db.conn);
        drop(src);
        let r2 = r.and_then(|_| {
            ddl_after(&mut db.conn, &tables, &end, &fresh)?;
            rename_indexes(&mut db.conn, &old, &schema, &existing)?;
            // indexes: in the transaction on SQLite and for the tables made here; the others
            // on PostgreSQL after the commit, without blocking writes
            let all: BTreeSet<String> = schema.tables.iter().map(|t| t.name.clone()).collect();
            let now: BTreeSet<String> = if d == Dialect::Sqlite {
                all.clone()
            } else {
                fresh.clone()
            };
            let (created, dropped) = db.sync_indexes(Some(&now), false)?;
            // (indexes built again with a copied table are not new)
            out.indexes_created.extend(
                created
                    .into_iter()
                    .filter(|n| !existing.iter().any(|(e, _, _)| e == n)),
            );
            out.indexes_dropped.extend(
                dropped
                    .into_iter()
                    .filter(|n| !schema.indexes.iter().any(|i| &i.name == n)),
            );
            if d == Dialect::Postgres {
                concurrent = all.difference(&fresh).cloned().collect();
                // the indexes no longer wanted go now
                let existing = db.existing_indexes()?;
                for (name, table, _) in existing {
                    if concurrent.contains(&table) && !schema.indexes.iter().any(|i| i.name == name)
                    {
                        db.conn.batch(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
                        out.indexes_dropped.push(name);
                    }
                }
            }
            let fp = schema.fingerprint();
            db.set_meta("schema_fingerprint", &fp)?;
            let names: Vec<String> = schema.tables.iter().map(|t| t.name.clone()).collect();
            db.set_meta("schema_tables", &serde_json::to_string(&names).unwrap())?;
            let mut cat = new.clone();
            cat.indexes = schema.indexes.clone();
            cat.write(&mut db)
        });
        let finished = match r2 {
            Ok(()) if !dry => db.conn.batch("COMMIT"),
            Ok(()) => db.conn.batch("ROLLBACK"),
            Err(e) => {
                let _ = db.conn.batch("ROLLBACK");
                Err(e)
            }
        };
        if copies {
            let _ = db.conn.batch("PRAGMA foreign_keys = ON");
        }
        finished?;
        if d == Dialect::Postgres && !dry {
            // built concurrently: an interrupted build is left invalid and built again by the
            // next deploy (or `quadra serve` at start)
            match db.ensure_indexes(Some(&concurrent)) {
                Ok((c, _)) => out.indexes_created.extend(c),
                Err(e) => out.index_errors.push(e),
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        return Err(e);
    }
    out.steps = steps;
    Ok(Ok(Done::Report(out)))
}
