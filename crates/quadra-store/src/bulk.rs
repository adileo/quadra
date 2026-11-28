//! Bulk import: large amounts of data loaded in one database transaction, without going
//! through the engine one record at a time.
//!
//! The rows of each entity (CSV files, or a generator) are decoded against the model and
//! validated as a commit would validate them: defaults, required values, `where`, `check`,
//! `warn`, `unique`, `frozen when`. Their materialized formulas are computed, then
//! everything is written in bulk: the history and the records in a few large commits of the
//! hash chain, the projection tables filled directly (a prepared statement on SQLite,
//! `COPY` on PostgreSQL). The result is the database the engine would have written
//! committing the same records: `quadra db verify` passes.
//!
//! The rules are evaluated by the engine itself, over the imported data held in memory (one
//! compact row of values per record) and served through the `Source` interface, with
//! in-memory indexes for the inverse relations. Records already in the database are loaded
//! too: their formulas and warnings are recomputed with the new data.
//!
//! Not imported (refused with a message): entities that post to ledgers, per-company
//! fields, value records (`record`) and fields of other types without a text form.
//! Reactions and automatic transitions do not run: the rows are the state of record.

use crate::codec::{cause_to_json, to_json};
use crate::conn::{Conn, Dialect, Pooled, Sql};
use crate::db::{encode, materialized_formulas, CommitHasher, Database};
use crate::schema::{self, Schema, Source as TableSource};
use chrono::{NaiveDate, NaiveDateTime};
use quadra_ir::{EntityKind, FieldKind, Model, Ty};
use quadra_runtime::store::{LedgerRow, Record, Source};
use quadra_runtime::{Cause, Ctx, Flow, Id, Interp, SharedSource, Store, V};
use quadra_types::{Num, Report, Unit, T};
use rust_decimal::Decimal;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How the cells of a field are read.
#[derive(Clone, Debug)]
enum Kind {
    Bool,
    Int,
    Dec,
    Pct,
    Text,
    Date,
    DateTime,
    YearMonth,
    Role,
    Money(Option<Rc<str>>),
    Price(Option<Rc<str>>, Option<Rc<str>>),
    Qty(Option<Rc<str>>),
    Variant(Vec<String>),
    /// a reference to an entity (or to any implementor of a trait)
    Ref(String),
    /// a list or set (`;` between the elements)
    Coll(Box<Kind>, bool),
}

impl Kind {
    fn target(&self) -> Option<&str> {
        match self {
            Kind::Ref(t) => Some(t),
            Kind::Coll(k, _) => k.target(),
            _ => None,
        }
    }
}

fn fixed(u: &Unit) -> Option<Rc<str>> {
    match u {
        Unit::Const(c) => Some(Rc::from(c.as_str())),
        _ => None,
    }
}

fn kind_of(model: &Model, t: &T) -> Option<Kind> {
    Some(match t {
        T::Opt(i) => return kind_of(model, i),
        T::Bool => Kind::Bool,
        T::Num(Num::Int) => Kind::Int,
        T::Num(Num::Percent) => Kind::Pct,
        T::Num(Num::Decimal) | T::Zero => Kind::Dec,
        T::Text(_) => Kind::Text,
        T::Date => Kind::Date,
        T::DateTime => Kind::DateTime,
        T::YearMonth => Kind::YearMonth,
        T::Role => Kind::Role,
        T::Money { cur, .. } => Kind::Money(fixed(cur)),
        T::Price { cur, per, .. } => Kind::Price(fixed(cur), fixed(per)),
        T::Qty { unit, .. } => Kind::Qty(fixed(unit)),
        T::Enum { variants, .. } => Kind::Variant(variants.clone()),
        T::Ent(e) => match model.entity(e)?.kind {
            EntityKind::Entity | EntityKind::Trait => Kind::Ref(e.clone()),
            EntityKind::Record => return None,
        },
        T::Coll { elem, set, .. } => Kind::Coll(Box::new(kind_of(model, elem)?), *set),
        _ => return None,
    })
}

struct FieldPlan {
    name: String,
    kind: Rc<Kind>,
    has_default: bool,
    /// a value is required (not optional, no `required when`)
    required: bool,
}

/// One record of the import: its stored values (in the order of `Plan::fields`) and what
/// the evaluation adds.
struct Row {
    vals: Vec<V>,
    /// fields whose default is still to compute (bit i: `fields[i]`)
    pending: u64,
    /// fields whose value came from their default (written to the history even when none)
    defaulted: u64,
    /// materialized formulas, in the order of `Plan::formulas`
    formulas: Vec<V>,
    warnings: Vec<String>,
    frozen: bool,
}

/// The rows of one entity: ids `first..first + rows.len()`, in the order given.
struct Plan {
    entity: String,
    /// stored fields, by name (the order of their history entries)
    fields: Vec<FieldPlan>,
    formulas: Vec<String>,
    first: Id,
    rows: Vec<Row>,
    /// the key of each row (the `id` column) → its record id
    keys: HashMap<Box<str>, Id>,
    /// which column of the file is which field (None: the key)
    columns: Vec<Option<usize>>,
    file: String,
}

impl Plan {
    fn field(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

/// Where a cell is: plan, row, field, line of the file.
struct Cell {
    plan: usize,
    row: usize,
    field: usize,
    line: usize,
}

/// A reference to a row whose key was not known yet when the cell was read.
struct Pending {
    plan: usize,
    row: usize,
    field: usize,
    /// position in a list or set
    elem: Option<usize>,
    target: String,
    key: String,
    place: String,
}

enum Index {
    /// target id → records whose field points to it
    Refs(HashMap<Id, Vec<Id>>),
}

/// The imported data and the records already in the database, served to the engine.
struct Mem {
    plans: Vec<Plan>,
    existing: HashMap<Id, Record>,
    existing_ids: HashMap<String, Vec<Id>>,
    counters: HashMap<String, u64>,
    base: u64,
    time: NaiveDateTime,
    cause: Rc<Cause>,
    /// entity → field → the index of the field (None: it holds no references)
    indexes: HashMap<String, HashMap<String, Option<Index>>>,
    /// the evaluation tried to create records (a default that creates one)
    created: bool,
}

impl Mem {
    fn locate(&self, id: Id) -> Option<(usize, usize)> {
        // plans are in ascending order of ids
        let p = self
            .plans
            .partition_point(|p| p.first <= id)
            .checked_sub(1)?;
        let plan = &self.plans[p];
        let r = (id - plan.first) as usize;
        (r < plan.rows.len()).then_some((p, r))
    }

    fn build(&self, p: usize, r: usize) -> Record {
        let plan = &self.plans[p];
        let row = &plan.rows[r];
        let mut fields =
            rustc_hash::FxHashMap::with_capacity_and_hasher(plan.fields.len(), Default::default());
        let mut pending = Vec::new();
        for (i, f) in plan.fields.iter().enumerate() {
            if row.pending & (1 << i) != 0 {
                pending.push(f.name.clone());
                continue;
            }
            let v = &row.vals[i];
            if !v.is_none() || row.defaulted & (1 << i) != 0 {
                fields.insert(f.name.clone(), v.clone());
            }
        }
        // (no history: the evaluation reads every record as of now, and what the import writes
        // to the history comes from the rows, `HistoryRows`)
        let history = Vec::new();
        Record {
            ent: Rc::from(plan.entity.as_str()),
            id: plan.first + r as u64,
            fields,
            history,
            created_tx: self.base,
            created_at: self.time,
            created_by: None,
            pending_defaults: pending,
            // the evaluation reads every record as of now: nothing comes after the import
            frozen_at: None,
            cause: self.cause.clone(),
            warnings: row.warnings.clone(),
            partial: None,
            formulas: HashMap::new(),
        }
    }

    fn index(&mut self, ent: &str, field: &str) -> &Option<Index> {
        if let Some(i) = self.indexes.get(ent).and_then(|m| m.get(field)) {
            // (the borrow checker wants the lookup again to return it)
            let _ = i;
            return &self.indexes[ent][field];
        }
        {
            let mut map: HashMap<Id, Vec<Id>> = HashMap::new();
            let mut ok = false;
            if let Some(p) = self.plans.iter().position(|p| p.entity == ent) {
                let plan = &self.plans[p];
                if let Some(i) = plan
                    .field(field)
                    .filter(|i| plan.fields[*i].kind.target().is_some())
                {
                    ok = true;
                    for (r, row) in plan.rows.iter().enumerate() {
                        for t in ref_ids(&row.vals[i]) {
                            map.entry(t).or_default().push(plan.first + r as u64);
                        }
                    }
                }
            }
            if let Some(ids) = self.existing_ids.get(ent) {
                for id in ids {
                    if let Some(v) = self.existing[id].fields.get(field) {
                        let targets = ref_ids(v);
                        ok |= !targets.is_empty();
                        for t in targets {
                            map.entry(t).or_default().push(*id);
                        }
                    }
                }
            }
            self.indexes
                .entry(ent.to_string())
                .or_default()
                .insert(field.to_string(), ok.then(|| Index::Refs(map)));
        }
        &self.indexes[ent][field]
    }
}

fn ref_ids(v: &V) -> Vec<Id> {
    match v {
        V::Ref { id, .. } => vec![*id],
        V::Set(items) | V::List(items) => items.iter().filter_map(|i| i.as_ref_id()).collect(),
        _ => Vec::new(),
    }
}

impl Source for Mem {
    fn base(&self) -> u64 {
        self.base
    }

    fn record(&mut self, id: Id) -> Option<Record> {
        if let Some(r) = self.existing.get(&id) {
            return Some(r.clone());
        }
        let (p, r) = self.locate(id)?;
        Some(self.build(p, r))
    }

    fn ids(&mut self, ent: &str) -> Vec<Id> {
        let mut out: Vec<Id> = self.existing_ids.get(ent).cloned().unwrap_or_default();
        if let Some(p) = self.plans.iter().find(|p| p.entity == ent) {
            out.extend(p.first..p.first + p.rows.len() as u64);
        }
        out
    }

    fn find(&mut self, ent: &str, field: &str, value: &V) -> Option<Vec<Id>> {
        let V::Ref { id, .. } = value else {
            return None;
        };
        match self.index(ent, field) {
            Some(Index::Refs(m)) => Some(m.get(id).cloned().unwrap_or_default()),
            None => None,
        }
    }

    fn rows(&mut self, _ledger: &str, _filter: Option<(&str, &V)>) -> Vec<LedgerRow> {
        Vec::new()
    }

    fn row(&mut self, _id: Id) -> Option<LedgerRow> {
        None
    }

    fn postings(&mut self, _record: Id) -> HashMap<String, Vec<HashMap<String, V>>> {
        HashMap::new()
    }

    fn counter(&mut self, key: &str) -> u64 {
        self.counters.get(key).copied().unwrap_or(0)
    }

    fn commit(&mut self, _tx: u64) -> Option<(NaiveDateTime, Option<Id>)> {
        Some((self.time, None))
    }

    fn reserve_ids(&mut self, n: u64) -> Id {
        // records created by the evaluation would be lost: the import refuses them
        self.created = true;
        u64::MAX / 2 - n
    }

    fn error(&mut self) -> Option<String> {
        None
    }
}

/// What an import did.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// (entity, records)
    pub entities: Vec<(String, usize)>,
    pub records: usize,
    pub history: u64,
    pub commits: Vec<u64>,
    pub warnings: usize,
    /// (phase, time)
    pub phases: Vec<(String, Duration)>,
}

/// An import being prepared: rows are added entity by entity, then `run` validates and
/// writes everything.
pub struct Import<'m> {
    model: &'m Model,
    report: &'m Report,
    mem: Mem,
    next_id: Id,
    pending: Vec<Pending>,
    started: Instant,
    pub records_per_commit: usize,
}

impl<'m> Import<'m> {
    /// Over the database's current data (loaded in memory: meant for initial loads).
    pub fn new(
        db: &mut Database,
        model: &'m Model,
        report: &'m Report,
        time: NaiveDateTime,
        label: &str,
    ) -> Result<Import<'m>, String> {
        let status = db.status()?;
        if !status.initialized {
            return Err("the database is not initialized (run `quadra db init`)".into());
        }
        if !status.schema_current {
            return Err("the schema changed: run `quadra db migrate` first".into());
        }
        let store = db.load()?;
        let mut existing_ids: HashMap<String, Vec<Id>> = HashMap::new();
        for r in store.records.values() {
            existing_ids
                .entry(r.ent.to_string())
                .or_default()
                .push(r.id);
        }
        let cause = Rc::new(Cause {
            desc: format!("bulk import {label}"),
            user: "system".into(),
            time,
            parent: None,
        });
        let mem = Mem {
            plans: Vec::new(),
            existing: store.records.clone().into_iter().collect(),
            existing_ids,
            counters: store.counters.clone(),
            base: store.tx,
            time,
            cause,
            indexes: HashMap::new(),
            created: false,
        };
        Ok(Import {
            model,
            report,
            mem,
            next_id: store.next_id,
            pending: Vec::new(),
            started: Instant::now(),
            records_per_commit: 250_000,
        })
    }

    /// The rows of `entity` follow, with these columns (`id`: the key other rows refer to).
    /// `file`: where they come from, for messages.
    pub fn entity(&mut self, entity: &str, header: &[String], file: &str) -> Result<usize, String> {
        let model = self.model;
        let e = model
            .entity(entity)
            .filter(|e| e.kind == EntityKind::Entity && !e.temporary)
            .ok_or_else(|| format!("{file}: unknown entity `{entity}`"))?;
        if e.is_ledger {
            return Err(format!(
                "{file}: `{entity}` is a ledger: its rows come from `post to`"
            ));
        }
        if !e.posts.is_empty() {
            return Err(format!(
                "{file}: `{entity}` posts to a ledger: the import does not compute postings yet"
            ));
        }
        if self.mem.plans.iter().any(|p| p.entity == entity) {
            return Err(format!("{file}: the rows of `{entity}` were already given"));
        }
        let mut fields = Vec::new();
        for f in model.all_fields(e) {
            if f.formula.is_some() || f.per_company || schema::is_inverse(model, e, f) {
                continue;
            }
            let t = self
                .report
                .field_types
                .get(&(e.name.clone(), f.name.clone()))
                .cloned()
                .or_else(|| {
                    model.all_traits(e).iter().find_map(|tr| {
                        self.report
                            .field_types
                            .get(&(tr.name.clone(), f.name.clone()))
                            .cloned()
                    })
                });
            let kind = t.as_ref().and_then(|t| kind_of(model, t));
            match kind {
                Some(kind) => {
                    let optional = matches!(
                        f.ty,
                        Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_)) | None
                    ) || f.required_when.is_some();
                    fields.push(FieldPlan {
                        name: f.name.clone(),
                        kind: Rc::new(kind),
                        has_default: f.default.is_some(),
                        required: !optional && f.kind == FieldKind::Stored,
                    })
                }
                None if header.iter().any(|h| h == &f.name) => {
                    return Err(format!(
                        "{file}: `{entity}.{}` has a type the import cannot read yet",
                        f.name
                    ))
                }
                None => {}
            }
        }
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        if fields.len() > 64 {
            return Err(format!("{file}: `{entity}` has more than 64 stored fields: not supported by the import yet"));
        }
        let mut columns = Vec::new();
        for h in header {
            if h == "id" {
                columns.push(None);
                continue;
            }
            match fields.iter().position(|f| &f.name == h) {
                Some(i) => columns.push(Some(i)),
                None => {
                    let why = match model.find_field(e, h) {
                        Some(f) if f.formula.is_some() => "is a formula: it is computed",
                        Some(f) if f.per_company => {
                            "is per company: not supported by the import yet"
                        }
                        Some(_) => "is computed from the other side of the relation",
                        None => "is not a field",
                    };
                    return Err(format!("{file}: column `{h}`: `{entity}.{h}` {why}"));
                }
            }
        }
        let formulas: Vec<String> = materialized_formulas(&schema::build(model, self.report))
            .into_iter()
            .filter(|(en, _)| en == entity)
            .map(|(_, f)| f)
            .collect();
        self.mem.plans.push(Plan {
            entity: entity.to_string(),
            fields,
            formulas,
            first: self.next_id + 1,
            rows: Vec::new(),
            keys: HashMap::new(),
            columns,
            file: file.to_string(),
        });
        Ok(self.mem.plans.len() - 1)
    }

    /// One row of the entity begun with `entity` (`h`), its cells in the order of the header.
    /// `line`: where it comes from, for messages.
    pub fn row(&mut self, h: usize, cells: &[&str], line: usize) -> Result<(), String> {
        let ncols = self.mem.plans[h].columns.len();
        if cells.len() != ncols {
            return Err(format!(
                "{} line {line}: {} cells, the header has {ncols}",
                self.mem.plans[h].file,
                cells.len()
            ));
        }
        let n = self.mem.plans[h].fields.len();
        let r = self.mem.plans[h].rows.len();
        let id = self.next_id + 1;
        let mut vals = vec![V::None; n];
        let mut given = 0u64;
        let mut key = None;
        for (c, cell) in cells.iter().enumerate() {
            let cell = cell.trim();
            let Some(i) = self.mem.plans[h].columns[c] else {
                key = Some(cell);
                continue;
            };
            if cell.is_empty() {
                continue;
            }
            given |= 1 << i;
            let kind = self.mem.plans[h].fields[i].kind.clone();
            let at = Cell {
                plan: h,
                row: r,
                field: i,
                line,
            };
            vals[i] = match &*kind {
                Kind::Coll(inner, set) => {
                    let mut items: Vec<V> = Vec::new();
                    for part in cell.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                        let v = self.value(inner, part, &at, Some(items.len()))?;
                        if !*set || !items.contains(&v) || matches!(v, V::Ref { id: 0, .. }) {
                            items.push(v);
                        }
                    }
                    if *set {
                        V::Set(Rc::new(items))
                    } else {
                        V::List(Rc::new(items))
                    }
                }
                k => self.value(k, cell, &at, None)?,
            };
        }
        let plan = &mut self.mem.plans[h];
        let pending = plan
            .fields
            .iter()
            .enumerate()
            .filter(|(i, f)| f.has_default && given & (1 << i) == 0)
            .fold(0u64, |m, (i, _)| m | 1 << i);
        if let Some(k) = key.filter(|k| !k.is_empty()) {
            if plan.keys.insert(Box::from(k), id).is_some() {
                return Err(format!(
                    "{} line {line}: the key `{k}` is used twice",
                    plan.file
                ));
            }
        }
        plan.rows.push(Row {
            vals,
            pending,
            defaulted: pending,
            formulas: Vec::new(),
            warnings: Vec::new(),
            frozen: false,
        });
        self.next_id = id;
        Ok(())
    }

    /// `file line N: field` for messages.
    fn place(&self, at: &Cell) -> String {
        let p = &self.mem.plans[at.plan];
        format!("{} line {}: {}", p.file, at.line, p.fields[at.field].name)
    }

    fn value(
        &mut self,
        kind: &Kind,
        cell: &str,
        at: &Cell,
        elem: Option<usize>,
    ) -> Result<V, String> {
        let bad = |s: &Self, what: &str| format!("{}: `{cell}` is not {what}", s.place(at));
        let dec = |s: &str| Decimal::from_str(s).ok();
        Ok(match kind {
            Kind::Bool => match cell {
                "true" | "yes" | "1" => V::Bool(true),
                "false" | "no" | "0" => V::Bool(false),
                _ => return Err(bad(self, "true or false")),
            },
            Kind::Int => V::Int(cell.parse().map_err(|_| bad(self, "an integer"))?),
            Kind::Dec => V::Dec(dec(cell).ok_or_else(|| bad(self, "a number"))?),
            Kind::Pct => V::Pct(
                cell.strip_suffix('%')
                    .and_then(|n| dec(n.trim()))
                    .ok_or_else(|| bad(self, "a percentage (`10%`)"))?
                    / Decimal::from(100),
            ),
            Kind::Text => V::Text(Rc::from(cell)),
            Kind::Date => {
                V::Date(NaiveDate::from_str(cell).map_err(|_| bad(self, "a date (YYYY-MM-DD)"))?)
            }
            Kind::DateTime => V::DateTime(
                NaiveDateTime::parse_from_str(cell, "%Y-%m-%dT%H:%M:%S")
                    .or_else(|_| NaiveDateTime::parse_from_str(cell, "%Y-%m-%d %H:%M:%S"))
                    .map_err(|_| bad(self, "a date and time (YYYY-MM-DDTHH:MM:SS)"))?,
            ),
            Kind::YearMonth => {
                let (y, m) = cell
                    .split_once('-')
                    .ok_or_else(|| bad(self, "a month (YYYY-MM)"))?;
                V::YearMonth(
                    y.parse().map_err(|_| bad(self, "a month (YYYY-MM)"))?,
                    m.parse().map_err(|_| bad(self, "a month (YYYY-MM)"))?,
                )
            }
            Kind::Role => {
                if !self.model.roles.iter().any(|x| x.name == cell) {
                    return Err(bad(self, "a role"));
                }
                V::Role(Rc::from(cell))
            }
            Kind::Variant(vs) => {
                if !vs.iter().any(|v| v == cell) {
                    return Err(format!(
                        "{}: `{cell}` is not one of {}",
                        self.place(at),
                        vs.join(", ")
                    ));
                }
                V::Variant {
                    name: Rc::from(cell),
                    fields: Rc::new(Vec::new()),
                }
            }
            Kind::Money(cur) => {
                let (amt, c) =
                    amount_unit(cell, cur).ok_or_else(|| bad(self, "an amount (`12.50 EUR`)"))?;
                V::Money { amt, cur: c }
            }
            Kind::Qty(unit) => {
                let (v, u) =
                    amount_unit(cell, unit).ok_or_else(|| bad(self, "a quantity (`3 pcs`)"))?;
                V::Qty { v, unit: u }
            }
            Kind::Price(cur, per) => {
                let (amt, rest) = cell
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| bad(self, "a price (`1.50 EUR/pcs`)"))?;
                let (c, p) = rest
                    .trim()
                    .split_once('/')
                    .ok_or_else(|| bad(self, "a price (`1.50 EUR/pcs`)"))?;
                let amt = dec(amt).ok_or_else(|| bad(self, "a price (`1.50 EUR/pcs`)"))?;
                for (given, expected) in [(c, cur), (p, per)] {
                    if expected.as_deref().is_some_and(|e| e != given) {
                        return Err(format!(
                            "{}: `{cell}`: expected a price in {}/{}",
                            self.place(at),
                            cur.as_deref().unwrap_or("?"),
                            per.as_deref().unwrap_or("?")
                        ));
                    }
                }
                V::Price {
                    amt,
                    cur: Rc::from(c),
                    per: Rc::from(p),
                }
            }
            Kind::Ref(target) => self.reference(target, cell, at, elem)?,
            Kind::Coll(..) => {
                return Err(format!(
                    "{}: nested collections are not supported",
                    self.place(at)
                ))
            }
        })
    }

    /// `#123` (a record already in the database), `key` (a row of the target entity) or
    /// `Entity:key` (a row of an entity implementing the target trait).
    fn reference(
        &mut self,
        target: &str,
        cell: &str,
        at: &Cell,
        elem: Option<usize>,
    ) -> Result<V, String> {
        let fits = |ent: &str| {
            ent == target
                || self
                    .model
                    .entity(ent)
                    .is_some_and(|e| self.model.implements(e, target))
        };
        if let Some(id) = cell.strip_prefix('#') {
            let id: Id = id
                .parse()
                .map_err(|_| format!("{}: `{cell}` is not a record id", self.place(at)))?;
            let rec = self.mem.existing.get(&id).ok_or_else(|| {
                format!(
                    "{}: there is no record #{id} in the database",
                    self.place(at)
                )
            })?;
            if !fits(&rec.ent) {
                return Err(format!(
                    "{}: #{id} is a {}, not a {target}",
                    self.place(at),
                    rec.ent
                ));
            }
            return Ok(V::Ref {
                ent: rec.ent.clone(),
                id,
            });
        }
        let (ent, key) = match cell.split_once(':') {
            Some((e, k)) if self.model.entity(e).is_some() => (e.to_string(), k.to_string()),
            _ => (target.to_string(), cell.to_string()),
        };
        if !fits(&ent) {
            return Err(format!("{}: {ent} is not a {target}", self.place(at)));
        }
        if let Some(id) = self
            .mem
            .plans
            .iter()
            .find(|p| p.entity == ent)
            .and_then(|p| p.keys.get(key.as_str()))
        {
            return Ok(V::Ref {
                ent: Rc::from(ent.as_str()),
                id: *id,
            });
        }
        // a row given later: resolved at the end
        let place = self.place(at);
        self.pending.push(Pending {
            plan: at.plan,
            row: at.row,
            field: at.field,
            elem,
            target: ent.clone(),
            key,
            place,
        });
        Ok(V::Ref {
            ent: Rc::from(ent.as_str()),
            id: 0,
        })
    }

    fn resolve(&mut self) -> Result<(), String> {
        for p in std::mem::take(&mut self.pending) {
            let id = self
                .mem
                .plans
                .iter()
                .find(|x| x.entity == p.target)
                .and_then(|x| x.keys.get(p.key.as_str()))
                .copied();
            let Some(id) = id else {
                let given = self.mem.plans.iter().any(|x| x.entity == p.target);
                return Err(if given {
                    format!("{}: no {} has the key `{}`", p.place, p.target, p.key)
                } else {
                    format!("{}: `{}`: the import has no {} rows (refer to a record of the database as #id)", p.place, p.key, p.target)
                });
            };
            let slot = &mut self.mem.plans[p.plan].rows[p.row].vals[p.field];
            let r = V::Ref {
                ent: Rc::from(p.target.as_str()),
                id,
            };
            match (p.elem, slot) {
                (None, slot) => *slot = r,
                (Some(i), V::Set(items) | V::List(items)) => Rc::make_mut(items)[i] = r,
                _ => {}
            }
        }
        Ok(())
    }

    /// Validate, compute and write. `progress` receives a line per phase.
    pub fn run(
        mut self,
        mut db: Database,
        progress: &mut dyn FnMut(&str),
    ) -> Result<ImportReport, String> {
        let mut out = ImportReport::default();
        let mut t = self.started;
        self.resolve()?;
        for p in &mut self.mem.plans {
            p.keys = HashMap::new();
        }
        // sets that held unresolved references: duplicates are dropped now
        for p in &mut self.mem.plans {
            for (i, f) in p.fields.iter().enumerate() {
                if let Kind::Coll(_, true) = *f.kind {
                    for row in &mut p.rows {
                        if let V::Set(items) = &mut row.vals[i] {
                            let mut seen = Vec::new();
                            Rc::make_mut(items).retain(|v| {
                                if seen.contains(v) {
                                    false
                                } else {
                                    seen.push(v.clone());
                                    true
                                }
                            });
                        }
                    }
                }
            }
        }
        out.records = self.mem.plans.iter().map(|p| p.rows.len()).sum();
        out.entities = self
            .mem
            .plans
            .iter()
            .map(|p| (p.entity.clone(), p.rows.len()))
            .collect();
        if out.records == 0 {
            return Err("nothing to import: the files have no rows".into());
        }
        phase("read", &mut t, &mut out, progress);

        let model = self.model;
        let time = self.mem.time;
        let mem = Rc::new(RefCell::new(self.mem));
        // the writes run on a thread of their own from the end of the defaults (the history
        // does not depend on the formulas or the rules): `Pipeline`
        let per_commit = self.records_per_commit;
        let mut pipe: Option<Pipeline> = None;
        let evaluated = {
            let mut hook = |h: Hook, m: &Mem| -> Result<(), String> {
                match h {
                    Hook::Defaults => pipe = Some(Pipeline::start(&mut db, model, m, per_commit)?),
                    Hook::Formulas => {
                        // (the writer may refuse a row, a key given twice: the rules, checked
                        // next, say why first; otherwise its error is reported at the end)
                        if let Some(p) = pipe.as_mut() {
                            let _ = p.projections(&db, model, m);
                        }
                    }
                    Hook::Record => {
                        if let Some(p) = pipe.as_mut() {
                            p.pump(m);
                        }
                    }
                }
                Ok(())
            };
            evaluate(
                model,
                self.report,
                &mem,
                time,
                &mut out,
                &mut t,
                progress,
                &mut hook,
            )
        };
        let (existing_formulas, existing_warnings) = match evaluated {
            Ok(x) => x,
            Err(e) => {
                if let Some(mut p) = pipe {
                    p.abandon(&mut db);
                }
                return Err(e);
            }
        };
        let Some(mut pipe) = pipe else {
            return Err("internal: the import's writer did not start".into());
        };

        // 4. writing: what is left
        let Ok(cell) = Rc::try_unwrap(mem) else {
            return Err("internal: the data is still in use".into());
        };
        let mem = cell.into_inner();
        out.warnings = mem
            .plans
            .iter()
            .flat_map(|p| p.rows.iter())
            .filter(|r| !r.warnings.is_empty())
            .count();
        let written = pipe.finish(&mut db, model, &mem);
        finish_write(
            &mut db,
            model,
            &mem,
            &pipe.prep,
            written,
            &existing_formulas,
            &existing_warnings,
            &mut out,
        )?;
        phase("write", &mut t, &mut out, progress);
        Ok(out)
    }
}

/// The end of a phase: its time, reported and kept.
fn phase(name: &str, since: &mut Instant, out: &mut ImportReport, progress: &mut dyn FnMut(&str)) {
    let d = since.elapsed();
    progress(&format!("  {name:<10} {:>8.1} s", d.as_secs_f64()));
    out.phases.push((name.to_string(), d));
    *since = Instant::now();
}

type Evaluated = (HashMap<(Id, String), V>, HashMap<Id, Vec<String>>);

/// Where the evaluation is, for the writes that go along with it (`Pipeline`).
enum Hook {
    /// the defaults are computed: the stored values are final
    Defaults,
    /// the formulas and `frozen when` are computed: the projection rows are final
    Formulas,
    /// a record was evaluated
    Record,
}

/// Defaults, rules and formulas, computed by the engine over the data in memory. Returns
/// the formulas and warnings of the records already in the database.
#[allow(clippy::too_many_arguments)]
fn evaluate(
    model: &Model,
    report: &Report,
    mem: &Rc<RefCell<Mem>>,
    time: NaiveDateTime,
    out: &mut ImportReport,
    t: &mut Instant,
    progress: &mut dyn FnMut(&str),
    hook: &mut dyn FnMut(Hook, &Mem) -> Result<(), String>,
) -> Result<Evaluated, String> {
    let shared: SharedSource = mem.clone();
    let mut interp = Interp::new(model);
    interp.set_clock(time);
    interp.store = Store::over(shared.clone());
    interp.store.untracked = true;
    interp.load_units();
    interp.system += 1;
    interp.company = interp.ids("Company", None).first().copied();
    let fail = |e: Flow| match e {
        Flow::Fail(m) => m,
        other => format!("{other:?}"),
    };
    let plans: Vec<(String, Id, usize)> = mem
        .borrow()
        .plans
        .iter()
        .map(|p| (p.entity.clone(), p.first, p.rows.len()))
        .collect();
    let label = |ent: &str, first: Id, id: Id| format!("{ent} row {}", id - first + 1);
    let existing_ids = mem.borrow().existing_ids.clone();
    // every record of an entity: those already in the database, then the imported rows
    let records_of = |ent: &str| -> Vec<(Id, Option<(usize, usize)>)> {
        let mut ids: Vec<(Id, Option<(usize, usize)>)> = existing_ids
            .get(ent)
            .map(|v| v.iter().map(|id| (*id, None)).collect())
            .unwrap_or_default();
        if let Some(p) = plans.iter().position(|(n, _, _)| n == ent) {
            ids.extend((0..plans[p].2).map(|r| (plans[p].1 + r as u64, Some((p, r)))));
        }
        ids
    };

    // 1. defaults, computed by the engine on the record as it is
    for (p, (ent, first, n)) in plans.iter().enumerate() {
        let mut ws = Working::new(&mut interp, &shared);
        let names: Vec<String> = mem.borrow().plans[p]
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();
        for r in 0..*n {
            let pending = mem.borrow().plans[p].rows[r].pending;
            if pending == 0 {
                continue;
            }
            let id = first + r as u64;
            let rec = V::Ref {
                ent: Rc::from(ent.as_str()),
                id,
            };
            let mut vals = Vec::new();
            for (i, name) in names.iter().enumerate() {
                if pending & (1 << i) != 0 {
                    let v = interp.get(&rec, name, None, None).map_err(|e| {
                        format!("{}: default of {name}: {}", label(ent, *first, id), fail(e))
                    })?;
                    vals.push((i, v));
                }
            }
            let mut m = mem.borrow_mut();
            let row = &mut m.plans[p].rows[r];
            for (i, v) in vals {
                row.vals[i] = v;
            }
            row.pending = 0;
            drop(m);
            ws.after(&mut interp, &shared);
        }
    }
    if !interp.store.counters.is_empty() {
        return Err(
            "a default numbers records (`next_number`): give the field in the file instead".into(),
        );
    }
    // the values the indexes were built from may have changed
    mem.borrow_mut().indexes.clear();
    // required values first: the rules of other records would only stumble on them
    let mut missing: Vec<String> = Vec::new();
    for p in mem.borrow().plans.iter() {
        for (r, row) in p.rows.iter().enumerate() {
            for (i, f) in p.fields.iter().enumerate() {
                if f.required && row.vals[i].is_none() && missing.len() <= 6 {
                    missing.push(format!(
                        "{}.{} is required ({})",
                        p.entity,
                        f.name,
                        label(&p.entity, p.first, p.first + r as u64)
                    ));
                }
            }
        }
    }
    if !missing.is_empty() {
        let more = if missing.len() > 6 { "; …" } else { "" };
        return Err(format!(
            "{}{more}",
            missing.into_iter().take(6).collect::<Vec<_>>().join("; ")
        ));
    }
    phase("defaults", t, out, progress);
    hook(Hook::Defaults, &mem.borrow())?;

    // 2. materialized formulas, the referencing entities first (lines before their
    // documents): with the data fixed, each formula of each record is computed once and the
    // rollups above reuse it
    interp.formula_memo = Some(HashMap::new());
    let targets = materialized_formulas(&schema::build(model, report));
    let field_names = |ent: &str| -> Vec<&str> {
        let e = model.entity(ent);
        targets
            .iter()
            .filter(|(en, _)| en == ent)
            .filter_map(|(_, f)| {
                e.and_then(|e| model.find_field(e, f))
                    .map(|f| f.name.as_str())
            })
            .collect()
    };
    let mut order: Vec<String> = plans.iter().rev().map(|p| p.0.clone()).collect();
    for (ent, _) in &targets {
        if !order.contains(ent) {
            order.push(ent.clone());
        }
    }
    for ent in &order {
        let names = field_names(ent);
        if names.is_empty() {
            continue;
        }
        let mut ws = Working::new(&mut interp, &shared);
        for (id, _) in records_of(ent) {
            let rec = V::Ref {
                ent: Rc::from(ent.as_str()),
                id,
            };
            for name in &names {
                // as the engine materializes them: an error leaves the column empty
                let _ = interp.get(&rec, name, None, None);
            }
            ws.after(&mut interp, &shared);
            hook(Hook::Record, &mem.borrow())?;
        }
    }
    // `frozen when` of the imported records (it may read their formulas), and the formulas to
    // the rows (the rules read them again from the memo): the projections are written while
    // the rules are checked. (A `frozen when` that fails is evaluated again where the rules
    // are, to fail there as it always did.)
    let entities: Vec<&quadra_ir::Entity> = model
        .entities
        .iter()
        .filter(|e| e.kind == EntityKind::Entity && !e.temporary && !e.is_ledger)
        .collect();
    let mut frozen_failed: HashSet<Id> = HashSet::new();
    for e in entities.iter().filter(|e| e.frozen.is_some()) {
        let fr = e.frozen.as_ref().unwrap();
        let mut ws = Working::new(&mut interp, &shared);
        for (id, at) in records_of(&e.name) {
            let Some((p, r)) = at else { continue };
            let rec = V::Ref {
                ent: Rc::from(e.name.as_str()),
                id,
            };
            match interp.eval(&fr.when, &mut record_ctx(&rec, &e.name)) {
                Ok(v) => mem.borrow_mut().plans[p].rows[r].frozen = v.truthy(),
                Err(_) => {
                    frozen_failed.insert(id);
                }
            }
            ws.after(&mut interp, &shared);
            hook(Hook::Record, &mem.borrow())?;
        }
    }
    if let Some(memo) = interp.formula_memo.as_ref() {
        let mut m = mem.borrow_mut();
        for p in m.plans.iter_mut() {
            let names = field_names(&p.entity);
            for (r, row) in p.rows.iter_mut().enumerate() {
                let id = p.first + r as u64;
                row.formulas = names
                    .iter()
                    .map(|n| memo.get(&(id, *n)).cloned().unwrap_or(V::None))
                    .collect();
            }
        }
    }
    phase("formulas", t, out, progress);
    hook(Hook::Formulas, &mem.borrow())?;

    // 3. rules, record by record, as the end of a transaction checks them
    interp.whole_data = true;
    interp.violations.clear();
    let mut missing: Vec<(Id, String)> = Vec::new();
    let mut unique_seen: HashMap<String, HashMap<String, Id>> = HashMap::new();
    let mut existing_warnings: HashMap<Id, Vec<String>> = HashMap::new();
    for e in &entities {
        let ids = records_of(&e.name);
        if ids.is_empty() {
            continue;
        }
        let mut ws = Working::new(&mut interp, &shared);
        let mut uniques: Vec<(Vec<String>, Option<&quadra_ir::Expr>)> = model
            .all_uniques(e)
            .iter()
            .map(|u| (u.fields.clone(), u.when.as_ref()))
            .collect();
        uniques.extend(
            model
                .all_fields(e)
                .into_iter()
                .filter(|f| f.unique)
                .map(|f| (vec![f.name.clone()], None)),
        );
        for (id, at) in ids {
            let before = missing.len();
            interp.check_record(e, id, &mut missing).map_err(|err| {
                format!(
                    "{}: {}",
                    at.map_or(format!("{}#{id}", e.name), |(p, _)| label(
                        &e.name, plans[p].1, id
                    )),
                    fail(err)
                )
            })?;
            for m in &mut missing[before..] {
                if let Some((p, _)) = at {
                    m.1 = format!("{} ({})", m.1, label(&e.name, plans[p].1, id));
                }
            }
            let rec = V::Ref {
                ent: Rc::from(e.name.as_str()),
                id,
            };
            let warnings = interp
                .store
                .records
                .get(&id)
                .map(|r| r.warnings.clone())
                .unwrap_or_default();
            let frozen = match (&e.frozen, at) {
                (Some(fr), Some(_)) if frozen_failed.contains(&id) => interp
                    .eval(&fr.when, &mut record_ctx(&rec, &e.name))
                    .map_err(|err| format!("{}: frozen when: {}", e.name, fail(err)))?
                    .truthy(),
                (Some(_), Some((p, r))) => mem.borrow().plans[p].rows[r].frozen,
                _ => false,
            };
            for (fields, when) in &uniques {
                if let Some(w) = when {
                    if !interp
                        .eval(w, &mut record_ctx(&rec, &e.name))
                        .map_err(fail)?
                        .truthy()
                    {
                        continue;
                    }
                }
                let mut key = Vec::new();
                let mut shown = Vec::new();
                for f in fields {
                    let v = interp.get(&rec, f, None, None).map_err(fail)?;
                    if v.is_none() {
                        break;
                    }
                    key.push(to_json(&v).to_string());
                    shown.push(interp.display(&v));
                }
                if key.len() < fields.len() {
                    continue;
                }
                let seen = unique_seen
                    .entry(format!(
                        "{}|{}|{}",
                        e.name,
                        fields.join(","),
                        when.is_some()
                    ))
                    .or_default();
                if seen.insert(key.join("\u{1}"), id).is_some() {
                    return Err(if fields.len() == 1 && when.is_none() {
                        format!(
                            "{}.{} must be unique: {} is given twice",
                            e.name, fields[0], shown[0]
                        )
                    } else {
                        format!(
                            "unique ({}): {} is given twice in {}",
                            fields.join(", "),
                            shown.join(", "),
                            e.name
                        )
                    });
                }
            }
            match at {
                Some((p, r)) => {
                    let mut m = mem.borrow_mut();
                    let row = &mut m.plans[p].rows[r];
                    row.warnings = warnings;
                    row.frozen = frozen;
                }
                None => {
                    if mem.borrow().existing[&id].warnings != warnings {
                        existing_warnings.insert(id, warnings);
                    }
                }
            }
            ws.after(&mut interp, &shared);
            hook(Hook::Record, &mem.borrow())?;
        }
        unique_seen.clear();
    }
    if !interp.violations.is_empty() {
        let report = interp.violations_report();
        let rest = report.split_once('\n').map(|(_, r)| r).unwrap_or("");
        return Err(format!(
            "the imported data breaks rules of the model:\n{rest}"
        ));
    }
    if !missing.is_empty() {
        let msgs: Vec<String> = missing.iter().map(|(_, m)| m.clone()).take(6).collect();
        return Err(format!(
            "{}{}",
            msgs.join("; "),
            if missing.len() > 6 {
                format!("; … {} more", missing.len() - 6)
            } else {
                String::new()
            }
        ));
    }
    if mem.borrow().created {
        return Err("a default or a formula creates records: the import cannot do that".into());
    }

    // the formulas of the records already in the database (those of the rows are there)
    let mut memo = interp.formula_memo.take().unwrap_or_default();
    drop(interp);
    let mut existing_formulas: HashMap<(Id, String), V> = HashMap::new();
    let m = mem.borrow_mut();
    for (ent, ids) in &existing_ids {
        for n in field_names(ent) {
            for id in ids {
                existing_formulas.insert(
                    (*id, n.to_string()),
                    memo.remove(&(*id, n)).unwrap_or(V::None),
                );
            }
        }
    }
    drop(m);
    phase("rules", t, out, progress);
    Ok((existing_formulas, existing_warnings))
}

/// The engine's working set during the evaluation: emptied when it grows (to bound the
/// memory: lookups in it no longer depend on its size), unless a few evaluations filled it
/// (they scan whole entities: what they load is worth keeping).
struct Working {
    limit: usize,
    evals: usize,
}

impl Working {
    fn new(interp: &mut Interp, shared: &SharedSource) -> Working {
        interp.store = Store::over(shared.clone());
        interp.store.untracked = true;
        Working {
            limit: 5_000,
            evals: 0,
        }
    }

    fn after(&mut self, interp: &mut Interp, shared: &SharedSource) {
        self.evals += 1;
        if interp.store.records.len() > self.limit {
            if self.evals < 50 {
                self.limit *= 4;
            }
            interp.store = Store::over(shared.clone());
            interp.store.untracked = true;
            self.evals = 0;
        }
    }
}

fn record_ctx(rec: &V, ent: &str) -> Ctx {
    Ctx {
        this: Some(rec.clone()),
        entity: Some(ent.to_string()),
        scopes: vec![Default::default()],
        company: None,
        as_of: None,
    }
}

/// `12.50 EUR`, `3 pcs`; the unit may be left out when the type fixes it.
fn amount_unit(cell: &str, fixed: &Option<Rc<str>>) -> Option<(Decimal, Rc<str>)> {
    let mut parts = cell.split_whitespace();
    let amt = Decimal::from_str(parts.next()?).ok()?;
    let unit: Rc<str> = match (parts.next(), fixed) {
        (Some(u), Some(f)) if u != &**f => return None,
        (Some(u), _) => Rc::from(u),
        (None, Some(f)) => f.clone(),
        (None, None) => return None,
    };
    parts.next().is_none().then_some((amt, unit))
}

fn ts(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// What the writes of an import need, read in its transaction before the data (`prepare`).
struct Prep {
    head: u64,
    /// the last `seq` of the history before the import
    seq: i64,
    /// the hash of the last commit before the import
    prev: String,
    per_commit: usize,
    last_tx: u64,
    at: String,
    cause_text: String,
    cause_json: String,
    /// the import is larger than the database: its indexes are built once their table is
    /// written (and on PostgreSQL its foreign keys checked once at the end)
    rebuild_indexes: bool,
    fk_dropped: Vec<String>,
    /// PostgreSQL: the system tables whose primary key is added back once they are written
    /// (an index kept up to date row by row costs more than one built at the end)
    pk_dropped: Vec<String>,
    /// the plans, for the rows of `_quadra_records` the writer makes (`write_records`)
    plans: Vec<PlanMeta>,
}

/// A plan as the writer sees it: its ids `first..first + len`, its entity.
#[derive(Clone)]
struct PlanMeta {
    first: u64,
    entity: String,
    len: usize,
    /// its records' rows are written once the formulas are (`frozen when` is known then);
    /// the others' as soon as the history is, while the formulas are computed
    late: bool,
}

impl Prep {
    fn tx_of(&self, global: usize) -> u64 {
        self.head + 1 + (global / self.per_commit) as u64
    }
}

/// The transaction of an import, begun, checked and made ready for the data: the indexes of
/// what it writes dropped when it is larger than the database (built again table by table).
fn prepare(db: &mut Database, mem: &Mem, per_commit: usize) -> Result<Prep, String> {
    let sqlite = db.dialect() == Dialect::Sqlite;
    if sqlite {
        // appended pages need no rollback journal: a WAL would grow as large as the data
        let _ = db.conn.batch("PRAGMA journal_mode = DELETE");
        // (64 MB: the pages beyond it go to the file while the writer inserts, not at the commit)
        let _ = db.conn.batch("PRAGMA cache_size = -65536");
    }
    db.conn
        .batch(if sqlite { "BEGIN IMMEDIATE" } else { "BEGIN" })?;
    let r = (|| -> Result<Prep, String> {
        if !sqlite {
            db.conn
                .batch("LOCK TABLE \"_quadra_commits\" IN EXCLUSIVE MODE")?;
        }
        let head = crate::source::head(&mut db.conn)?;
        if head != mem.base {
            return Err("the database changed while the import was prepared: run it again".into());
        }
        // an import larger than the database: its indexes are built once at the end of each
        // table (faster than kept up to date row by row), in the same transaction
        let total: usize = mem.plans.iter().map(|p| p.rows.len()).sum();
        let existing = db.conn.query(
            "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_records\"",
            &[],
        )?;
        let existing: usize = existing
            .first()
            .and_then(|r| r[0].as_deref())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let rebuild_indexes = total > existing;
        let mut fk_dropped: Vec<String> = Vec::new();
        let mut pk_dropped: Vec<String> = Vec::new();
        if rebuild_indexes {
            if !sqlite {
                db.conn.batch("SET LOCAL maintenance_work_mem = '512MB'")?;
                // (no foreign key refers to them)
                for t in ["_quadra_history", "_quadra_records"] {
                    for (name, _) in crate::deploy::pg_constraints(&mut db.conn, t, "p")? {
                        db.conn
                            .batch(&format!("ALTER TABLE \"{t}\" DROP CONSTRAINT \"{name}\""))?;
                        pk_dropped.push(t.to_string());
                    }
                }
            }
            // (SQLite: the import appends to the system indexes in their order — by record, by
            // transaction —: cheaper kept than built again)
            if !sqlite {
                for (name, _, _) in crate::db::SYSTEM_INDEXES {
                    db.conn.batch(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
                }
            }
            let tables: BTreeSet<String> = mem
                .plans
                .iter()
                .filter_map(|p| {
                    db.entity_tables
                        .get(&p.entity)
                        .map(|i| db.schema.tables[*i].name.clone())
                })
                .collect();
            for i in db
                .schema
                .indexes
                .clone()
                .iter()
                .filter(|i| tables.contains(&i.table) && !(sqlite && i.fts))
            {
                db.conn
                    .batch(&format!("DROP INDEX IF EXISTS \"{}\"", i.name))?;
            }
            if sqlite {
                // the full-text search is filled once a table is complete, not by a trigger per row
                for t in &tables {
                    crate::db::drop_search_triggers(&mut db.conn, t)?;
                }
            }
            if !sqlite {
                // the foreign keys of what is written too: checked once at the end, by one query
                // each, instead of a trigger per row
                let imported: BTreeSet<String> =
                    mem.plans.iter().map(|p| p.entity.clone()).collect();
                let names: Vec<String> = db
                    .schema
                    .tables
                    .iter()
                    .filter(|t| match &t.source {
                        TableSource::Entity(e)
                        | TableSource::Ledger(e)
                        | TableSource::Join(e, _)
                        | TableSource::PerCompany(e, _) => imported.contains(e),
                        TableSource::Trait(_) => true,
                    })
                    .map(|t| t.name.clone())
                    .collect();
                for t in &names {
                    for (name, _) in crate::deploy::pg_constraints(&mut db.conn, t, "f")? {
                        db.conn
                            .batch(&format!("ALTER TABLE \"{t}\" DROP CONSTRAINT \"{name}\""))?;
                        fk_dropped.push(t.clone());
                    }
                }
            }
        }
        let seq = db.conn.query(
            "SELECT CAST(MAX(\"seq\") AS TEXT) FROM \"_quadra_history\"",
            &[],
        )?;
        let seq: i64 = seq
            .first()
            .and_then(|r| r[0].as_deref())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let last = db.conn.query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1", &[])?;
        let prev = last
            .first()
            .and_then(|r| r[0].clone())
            .unwrap_or_else(|| "0".repeat(64));
        let per_commit = per_commit.max(1);
        let ncommits = total.div_ceil(per_commit).max(1);
        Ok(Prep {
            head,
            seq,
            prev,
            per_commit,
            last_tx: head + ncommits as u64,
            at: ts(mem.time),
            cause_text: mem.cause.chain().join(" ← "),
            cause_json: cause_to_json(&mem.cause).to_string(),
            rebuild_indexes,
            fk_dropped,
            pk_dropped,
            plans: Vec::new(),
        })
    })();
    if r.is_err() {
        let _ = db.conn.batch("ROLLBACK");
    }
    r
}

/// A history entry of the import for the writer: a new commit begins, or an entry of a
/// record (its id, field, value as the history has it).
enum HItem {
    Tx(u64),
    /// the next record: whether its row reads back exactly, its values of another kind of
    /// number (`ReadBack`), for its row of `_quadra_records`
    Record(bool, Option<String>),
    /// a value: (record, field, value, whether its entity has `history none`)
    Entry(i64, Arc<str>, String, bool),
}

/// What the main thread asks the import's writer thread to write, in order.
enum Job {
    History(Vec<HItem>),
    HistoryEnd,
    Rows {
        table: String,
        cols: Vec<String>,
        rows: Vec<Vec<Sql>>,
    },
    /// the formulas are computed: which records are frozen (by position in the import), for
    /// the rows of `_quadra_records` of the late plans
    Frozen(Vec<bool>),
    /// the rules are checked: the rows of `_quadra_records` written with another `frozen_tx`
    /// or with warnings: (id, frozen_tx, warnings)
    RecordsFix(Vec<(i64, Option<i64>, String)>),
    /// a table is complete: its indexes are built now (when the import builds them)
    Done(String),
    /// every projection table is complete: the foreign keys dropped are added back (checked)
    ForeignKeys,
    Finish,
}

/// The history entries of the import, in commit order (plan, row, field), produced a chunk at
/// a time: the writer inserts them while the formulas and rules are computed. Before each
/// record's entries, what its row of `_quadra_records` needs (`HItem::Record`).
struct Feeder {
    plan: usize,
    row: usize,
    global: usize,
    tx: Option<u64>,
    names: Vec<Vec<Arc<str>>>,
    /// per plan: its entity has `history none` (its values outside the chain)
    untracked: Vec<bool>,
    back: ReadBack,
    done: bool,
    /// entries sent and not written yet (the writer counts them down)
    pending: Arc<AtomicUsize>,
}

impl Feeder {
    fn new(db: &Database, mem: &Mem, pending: Arc<AtomicUsize>) -> Feeder {
        let names = mem
            .plans
            .iter()
            .map(|p| {
                p.fields
                    .iter()
                    .map(|f| Arc::from(f.name.as_str()))
                    .collect()
            })
            .collect();
        let untracked = mem
            .plans
            .iter()
            .map(|p| db.schema.untracked.contains(&p.entity))
            .collect();
        Feeder {
            plan: 0,
            row: 0,
            global: 0,
            tx: None,
            untracked,
            names,
            back: ReadBack::new(db, mem),
            done: false,
            pending,
        }
    }

    /// The next entries, about `max` of them (a record's are never split).
    fn produce(&mut self, mem: &Mem, prep: &Prep, max: usize) -> Vec<HItem> {
        let mut out = Vec::with_capacity(max + 16);
        while out.len() < max {
            let Some(p) = mem.plans.get(self.plan) else {
                self.done = true;
                break;
            };
            if self.row >= p.rows.len() {
                self.plan += 1;
                self.row = 0;
                continue;
            }
            let tx = prep.tx_of(self.global);
            if self.tx != Some(tx) {
                self.tx = Some(tx);
                out.push(HItem::Tx(tx));
            }
            let row = &p.rows[self.row];
            let id = (p.first + self.row as u64) as i64;
            out.push(HItem::Record(
                self.back.exact(self.plan, row),
                self.back.kinds(self.plan, p, row),
            ));
            for (i, v) in row.vals.iter().enumerate() {
                if v.is_none() && row.defaulted & (1 << i) == 0 {
                    continue;
                }
                let mut text = String::new();
                quadra_runtime::codec::write_json(v, &mut text);
                out.push(HItem::Entry(
                    id,
                    self.names[self.plan][i].clone(),
                    text,
                    self.untracked[self.plan],
                ));
            }
            self.row += 1;
            self.global += 1;
        }
        self.pending.fetch_add(out.len(), Ordering::Relaxed);
        out
    }
}

/// The entries the writer receives, as rows of `_quadra_history`: numbered, each commit
/// hashed as it streams (`CommitHasher`).
struct HistoryIn<'a> {
    jobs: &'a Receiver<Job>,
    buf: std::vec::IntoIter<HItem>,
    seq: i64,
    tx: u64,
    at: String,
    cause: String,
    prev: String,
    hasher: Option<(u64, CommitHasher)>,
    /// (tx, previous hash, hash)
    hashes: Vec<(u64, String, String)>,
    pending: Arc<AtomicUsize>,
    ended: bool,
    /// the main thread stopped (the evaluation failed): nothing is committed
    cancelled: bool,
    entry: String,
    /// per record, in order: whether its row reads back exactly; its kinds, by position
    exact: Vec<bool>,
    kinds: HashMap<usize, String>,
    /// the database's salts are seeded (tests: `Database::seed_salts`)
    seeded: bool,
}

impl HistoryIn<'_> {
    fn close(&mut self) {
        if let Some((tx, h)) = self.hasher.take() {
            let hash = h.finish();
            self.hashes
                .push((tx, std::mem::replace(&mut self.prev, hash.clone()), hash));
        }
    }
}

/// `s` as a JSON string (as serde_json writes it: `"` and `\` escaped, control characters as
/// `\n`, `\u00XX`…), appended to `out`.
fn json_str(out: &mut String, s: &str) {
    if s.bytes().any(|b| b < 0x20) {
        out.push_str(&serde_json::to_string(s).unwrap_or_default());
        return;
    }
    out.push('"');
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(esc);
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

impl HistoryIn<'_> {
    /// The next entry, numbered and hashed: (seq, tx, record, field, value, salt); None at the
    /// end.
    fn next_entry(&mut self) -> Option<(i64, u64, i64, Arc<str>, String, String)> {
        loop {
            if let Some(item) = self.buf.next() {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                match item {
                    HItem::Record(exact, kinds) => {
                        if let Some(k) = kinds {
                            self.kinds.insert(self.exact.len(), k);
                        }
                        self.exact.push(exact);
                    }
                    HItem::Tx(tx) => {
                        self.close();
                        self.tx = tx;
                        self.hasher = Some((
                            tx,
                            CommitHasher::new(&self.prev, tx, &self.at, None, &self.cause),
                        ));
                    }
                    HItem::Entry(id, field, value, true) => {
                        // `history none`: outside the chain, no digest
                        self.seq += 1;
                        return Some((
                            self.seq,
                            self.tx,
                            id,
                            field,
                            value,
                            crate::db::UNTRACKED.to_string(),
                        ));
                    }
                    HItem::Entry(id, field, value, false) => {
                        // `["h",id,"field","digest"]`, as `history_entry` has it
                        use std::fmt::Write as _;
                        self.seq += 1;
                        let salt = crate::db::new_salt(self.seeded, self.seq);
                        let digest = crate::db::value_digest(&salt, &value);
                        self.entry.clear();
                        let _ = write!(self.entry, "[\"h\",{id},");
                        json_str(&mut self.entry, &field);
                        let _ = write!(self.entry, ",\"{digest}\"]");
                        if let Some((_, h)) = self.hasher.as_mut() {
                            h.entry(&self.entry);
                        }
                        return Some((self.seq, self.tx, id, field, value, salt));
                    }
                }
                continue;
            }
            if self.ended || self.cancelled {
                return None;
            }
            match self.jobs.recv() {
                Ok(Job::History(items)) => self.buf = items.into_iter(),
                Ok(Job::HistoryEnd) => {
                    self.close();
                    self.ended = true;
                }
                _ => self.cancelled = true,
            }
        }
    }

    /// The next entry as a line of COPY text; false at the end.
    fn next_text(&mut self, buf: &mut String) -> bool {
        use std::fmt::Write as _;
        let Some((seq, tx, id, field, value, salt)) = self.next_entry() else {
            return false;
        };
        let _ = write!(buf, "{seq}\t{tx}\t{id}\t");
        crate::conn::copy_escape(buf, &field);
        buf.push('\t');
        crate::conn::copy_escape(buf, &value);
        let _ = write!(buf, "\t{salt}\n");
        true
    }
}

impl Iterator for HistoryIn<'_> {
    type Item = Vec<Sql>;

    fn next(&mut self) -> Option<Vec<Sql>> {
        let (seq, tx, id, field, value, salt) = self.next_entry()?;
        Some(vec![
            Sql::Int(seq),
            Sql::Int(tx as i64),
            Sql::Int(id),
            Sql::Text(field.to_string()),
            Sql::Text(value),
            Sql::Text(salt),
        ])
    }
}

/// What the writer thread wrote: the history rows, the commits (tx, previous hash, hash), the
/// tables it completed, whether it added the foreign keys back.
type Written = (u64, Vec<(u64, String, String)>, Vec<String>, bool);

/// The writer thread: the history, then the records' rows that do not wait for the formulas
/// (`PlanMeta::late`), then the rows of each table as they come, each table's indexes once it
/// is complete. It gives the connection back, inside the transaction.
fn writer(
    conn: Conn,
    schema: Schema,
    prep: &Prep,
    jobs: Receiver<Job>,
    pending: Arc<AtomicUsize>,
    queued: Arc<AtomicUsize>,
) -> (Conn, Result<Written, String>) {
    let mut db = Database::with(Pooled::own(conn), Rc::new(schema));
    let rebuild = prep.rebuild_indexes;
    let pg = db.dialect() == Dialect::Postgres;
    // a table written: its key and indexes; on PostgreSQL its statistics (in the transaction:
    // they hold once it commits)
    let indexes = |db: &mut Database, table: &str| -> Result<(), String> {
        if rebuild {
            if prep.pk_dropped.iter().any(|t| t == table) {
                let key = if table == "_quadra_history" {
                    "seq"
                } else {
                    "id"
                };
                db.conn.batch(&format!(
                    "ALTER TABLE \"{table}\" ADD PRIMARY KEY (\"{key}\")"
                ))?;
            }
            if table.starts_with("_quadra_") {
                for (n, t, cols) in crate::db::SYSTEM_INDEXES
                    .iter()
                    .filter(|(_, t, _)| *t == table)
                {
                    db.conn.batch(&format!(
                        "CREATE INDEX IF NOT EXISTS \"{n}\" ON \"{t}\" ({})",
                        cols.iter()
                            .map(|c| format!("\"{c}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))?;
                }
            } else {
                db.sync_indexes(Some(&BTreeSet::from([table.to_string()])), false)?;
            }
        }
        Ok(())
    };
    let stats = |db: &mut Database, table: &str| -> Result<(), String> {
        if pg {
            if table == "_quadra_history" {
                // the history is read by its indexes only: a small sample tells the planner so
                db.conn.batch("SET LOCAL default_statistics_target = 10")?;
                db.conn.batch("ANALYZE \"_quadra_history\"")?;
                db.conn
                    .batch("SET LOCAL default_statistics_target TO DEFAULT")?;
            } else {
                db.conn.batch(&format!("ANALYZE \"{table}\""))?;
            }
        }
        Ok(())
    };
    let complete = |db: &mut Database, table: &str| -> Result<(), String> {
        indexes(db, table)?;
        stats(db, table)
    };
    let r = (|| -> Result<Written, String> {
        let seeded = db.salts_seeded()?;
        let mut hist = HistoryIn {
            jobs: &jobs,
            buf: Vec::new().into_iter(),
            seq: prep.seq,
            tx: 0,
            at: prep.at.clone(),
            cause: prep.cause_text.clone(),
            prev: prep.prev.clone(),
            hasher: None,
            hashes: Vec::new(),
            pending,
            ended: false,
            cancelled: false,
            entry: String::new(),
            exact: Vec::new(),
            kinds: HashMap::new(),
            seeded,
        };
        let n = if pg {
            // COPY text written straight from the entries
            let mut n = 0u64;
            db.conn.copy_text(
                "_quadra_history",
                &["seq", "tx", "record_id", "field", "value", "salt"],
                &mut |buf| {
                    for _ in 0..1000 {
                        if !hist.next_text(buf) {
                            return Ok(false);
                        }
                        n += 1;
                    }
                    Ok(true)
                },
            )?;
            n
        } else {
            db.conn.bulk_insert(
                "_quadra_history",
                &["seq", "tx", "record_id", "field", "value", "salt"],
                &mut hist,
            )?
        };
        if hist.cancelled {
            return Err("cancelled".into());
        }
        let hashes = std::mem::take(&mut hist.hashes);
        complete(&mut db, "_quadra_history")?;
        // the records' rows that do not wait for the formulas, and their indexes when they are
        // most of them, while the formulas are computed
        let exact = std::mem::take(&mut hist.exact);
        let kinds = std::mem::take(&mut hist.kinds);
        write_records(&mut db, prep, &exact, &kinds, None)?;
        let early: usize = prep.plans.iter().filter(|p| !p.late).map(|p| p.len).sum();
        let late: usize = prep.plans.iter().filter(|p| p.late).map(|p| p.len).sum();
        let mut records_indexed = false;
        if early > 0 && early >= late {
            indexes(&mut db, "_quadra_records")?;
            records_indexed = true;
        }
        let mut done: Vec<String> = Vec::new();
        let mut fks = false;
        loop {
            let job = jobs.recv();
            if matches!(job, Ok(Job::Rows { .. } | Job::Done(_) | Job::ForeignKeys)) {
                // (the records' jobs, sent at the end, are not counted)
                let _ =
                    queued.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
            }
            match job {
                Ok(Job::Rows { table, cols, rows }) => {
                    let names: Vec<&str> = cols.iter().map(String::as_str).collect();
                    db.conn.bulk_insert(&table, &names, &mut rows.into_iter())?;
                }
                Ok(Job::Frozen(frozen)) => {
                    write_records(&mut db, prep, &exact, &kinds, Some(&frozen))?;
                }
                Ok(Job::RecordsFix(fix)) => {
                    if !records_indexed {
                        indexes(&mut db, "_quadra_records")?;
                        records_indexed = true;
                    }
                    fix_records(&mut db, &fix)?;
                }
                Ok(Job::Done(table)) if table == "_quadra_records" => {
                    if !records_indexed {
                        indexes(&mut db, &table)?;
                        records_indexed = true;
                    }
                    stats(&mut db, &table)?;
                    done.push(table);
                }
                Ok(Job::ForeignKeys) => {
                    if pg && rebuild {
                        // the deferred checks first: a table with pending checks cannot be altered
                        db.conn.batch("SET CONSTRAINTS ALL IMMEDIATE")?;
                        let mut fk = prep.fk_dropped.clone();
                        fk.dedup();
                        for t in &fk {
                            if let Some(table) = db.schema.table(t).cloned() {
                                for k in table.foreign_keys() {
                                    db.conn.batch(&k)?;
                                }
                            }
                        }
                    }
                    fks = true;
                }
                Ok(Job::Done(table)) => {
                    complete(&mut db, &table)?;
                    done.push(table.clone());
                }
                Ok(Job::Finish) => {
                    return Ok((n, hashes, done, fks));
                }
                Ok(_) => return Err("internal: an unexpected job of the import's writer".into()),
                Err(_) => return Err("cancelled".into()),
            }
        }
    })();
    let conn = db.conn.take_conn().expect("the import's connection");
    (conn, r)
}

/// How the stored values of each plan read back from their columns: whether a record's row
/// reads back exactly (`db::exact_row`, its `version_tx`) and the values held by columns of
/// another kind of number (`db::kinds_of`), for its row of `_quadra_records`.
struct ReadBack {
    /// per plan, per stored field: how it reads back (None: no column, nothing to read back);
    /// None: the entity's rows cannot be read back
    decodes: Vec<Option<Vec<Option<Back>>>>,
    /// per plan, per stored field: its column's decoding
    col_decodes: Vec<Vec<Option<crate::schema::Decode>>>,
}

enum Back {
    Cols(crate::schema::Decode),
    Refs(crate::schema::SetMap),
}

impl ReadBack {
    fn new(db: &Database, mem: &Mem) -> ReadBack {
        let decodes = mem
            .plans
            .iter()
            .map(|p| {
                let t = db
                    .schema
                    .entity_table(&p.entity)
                    .filter(|_| db.schema.readable.contains(&p.entity))?;
                let sets = db.schema.sets.get(&p.entity);
                Some(
                    p.fields
                        .iter()
                        .map(
                            |f| match sets.and_then(|s| s.iter().find(|m| m.field == f.name)) {
                                Some(sm) => Some(Back::Refs(sm.clone())),
                                None => t
                                    .fields
                                    .iter()
                                    .find(|fm| fm.field == f.name && !fm.formula)
                                    .map(|fm| Back::Cols(fm.decode.clone())),
                            },
                        )
                        .collect(),
                )
            })
            .collect();
        let col_decodes = mem
            .plans
            .iter()
            .map(|p| {
                let t = db.schema.entity_table(&p.entity);
                p.fields
                    .iter()
                    .map(|f| {
                        t.and_then(|t| t.fields.iter().find(|fm| fm.field == f.name && !fm.formula))
                            .map(|fm| fm.decode.clone())
                    })
                    .collect()
            })
            .collect();
        ReadBack {
            decodes,
            col_decodes,
        }
    }

    /// Whether the projection row of a record of plan `pi` reads back exactly.
    fn exact(&self, pi: usize, row: &Row) -> bool {
        self.decodes[pi].as_ref().is_some_and(|ds| {
            row.vals
                .iter()
                .zip(ds.iter())
                .enumerate()
                .all(|(i, (v, d))| {
                    // (a field without a value is not in the record: `Feeder`)
                    (v.is_none() && row.defaulted & (1 << i) == 0)
                        || match d {
                            None => true,
                            Some(Back::Cols(d)) => {
                                crate::db::fits(d, v) || crate::db::kind_note(d, v).is_some()
                            }
                            Some(Back::Refs(sm)) => crate::db::fits_refs(sm, v),
                        }
                })
        })
    }

    /// The `kinds` of a record's row: its values of another kind of number than their columns.
    fn kinds(&self, pi: usize, p: &Plan, row: &Row) -> Option<String> {
        let mut out = serde_json::Map::new();
        for (i, (v, d)) in row.vals.iter().zip(self.col_decodes[pi].iter()).enumerate() {
            if v.is_none() && row.defaulted & (1 << i) == 0 {
                continue;
            }
            if let Some(n) = d.as_ref().and_then(|d| crate::db::kind_note(d, v)) {
                out.insert(
                    p.fields[i].name.clone(),
                    serde_json::Value::String(n.to_string()),
                );
            }
        }
        (!out.is_empty()).then(|| serde_json::Value::Object(out).to_string())
    }
}

/// The writer: the rows of `_quadra_records` of the plans that are not late (`frozen` None:
/// none is frozen) or of the late ones (`frozen`: by position in the import), without warnings
/// (`fix_records` adds them). `exact` and `kinds` by position, as the history brought them.
fn write_records(
    db: &mut Database,
    prep: &Prep,
    exact: &[bool],
    kinds: &HashMap<usize, String>,
    frozen: Option<&[bool]>,
) -> Result<u64, String> {
    let late = frozen.is_some();
    let mut offset = 0usize;
    let parts: Vec<(usize, &PlanMeta)> = prep
        .plans
        .iter()
        .map(|p| {
            let o = offset;
            offset += p.len;
            (o, p)
        })
        .filter(|(_, p)| p.late == late && p.len > 0)
        .collect();
    if parts.is_empty() {
        return Ok(0);
    }
    let mut rows = parts
        .into_iter()
        .flat_map(|(o, p)| (0..p.len).map(move |r| (o + r, p, r)))
        .map(|(g, p, r)| {
            let tx = prep.tx_of(g) as i64;
            let is_frozen = frozen.is_some_and(|f| f.get(g).copied().unwrap_or(false));
            vec![
                Sql::Int((p.first + r as u64) as i64),
                Sql::Text(p.entity.clone()),
                Sql::Int(tx),
                Sql::Text(prep.at.clone()),
                Sql::Null,
                if is_frozen {
                    Sql::Int(prep.last_tx as i64)
                } else {
                    Sql::Null
                },
                Sql::Text(prep.cause_json.clone()),
                Sql::Text("[]".into()),
                Sql::Int(tx),
                Sql::Int(if exact.get(g).copied().unwrap_or(false) {
                    tx
                } else {
                    0
                }),
                kinds.get(&g).map_or(Sql::Null, |k| Sql::Text(k.clone())),
            ]
        });
    db.conn.bulk_insert(
        "_quadra_records",
        &[
            "id",
            "entity",
            "created_tx",
            "created_at",
            "created_by",
            "frozen_tx",
            "cause",
            "warnings",
            "last_tx",
            "version_tx",
            "kinds",
        ],
        &mut rows,
    )
}

/// The writer: the rows of `_quadra_records` whose `frozen_tx` or warnings the rules changed,
/// 10 000 per statement.
fn fix_records(db: &mut Database, fix: &[(i64, Option<i64>, String)]) -> Result<(), String> {
    let sql = match db.dialect() {
        Dialect::Postgres => {
            "UPDATE \"_quadra_records\" AS r SET \"frozen_tx\" = v.f, \"warnings\" = v.w FROM jsonb_to_recordset(CAST(?text AS JSONB)) AS v(id BIGINT, f BIGINT, w TEXT) WHERE r.\"id\" = v.id"
        }
        Dialect::Sqlite => {
            "UPDATE \"_quadra_records\" SET \"frozen_tx\" = v.f, \"warnings\" = v.w FROM (SELECT json_extract(value, '$.id') AS id, json_extract(value, '$.f') AS f, json_extract(value, '$.w') AS w FROM json_each(?text)) AS v WHERE \"_quadra_records\".\"id\" = v.id"
        }
    };
    for chunk in fix.chunks(10_000) {
        let rows: Vec<serde_json::Value> = chunk
            .iter()
            .map(|(id, f, w)| serde_json::json!({ "id": id, "f": f, "w": w }))
            .collect();
        db.conn.exec(
            sql,
            &[Sql::Text(serde_json::Value::Array(rows).to_string())],
        )?;
    }
    Ok(())
}

/// A part of the projection rows of the import: the rows of one table for one plan.
enum Seg {
    /// the entity table: (stored field or formula index, column) per column
    Entity {
        cols: Vec<(Option<(bool, usize)>, schema::Column)>,
    },
    /// a join table: the index of the set field
    Join(usize),
    /// a trait table
    Trait,
    /// a table is complete
    Done,
}

/// The projection rows of the import, produced a chunk at a time (`Pipeline::pump_rows`): per
/// entity its table, join tables and trait tables, each table declared complete after its rows.
struct Projections {
    /// (plan, table, column names, what)
    segs: Vec<(usize, String, Vec<String>, Seg)>,
    /// records of the plans before each plan
    offsets: Vec<usize>,
    seg: usize,
    row: usize,
    created_at: String,
}

impl Projections {
    fn new(db: &Database, model: &Model, mem: &Mem) -> Projections {
        let mut segs = Vec::new();
        let mut offsets = Vec::new();
        let mut offset = 0usize;
        let mut traits_written: BTreeSet<String> = BTreeSet::new();
        for (pi, p) in mem.plans.iter().enumerate() {
            offsets.push(offset);
            offset += p.rows.len();
            let Some(&ti) = db.entity_tables.get(&p.entity) else {
                continue;
            };
            let table = &db.schema.tables[ti];
            let mut names: Vec<String> = [
                "id",
                "_created_tx",
                "_created_at",
                "_created_by",
                "_frozen_tx",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let mut cols = Vec::new();
            for fm in &table.fields {
                let src = if fm.formula {
                    p.formulas
                        .iter()
                        .position(|f| *f == fm.field)
                        .map(|i| (true, i))
                } else {
                    p.field(&fm.field).map(|i| (false, i))
                };
                for c in &fm.columns {
                    names.push(c.name.clone());
                    cols.push((src, c.clone()));
                }
            }
            segs.push((pi, table.name.clone(), names, Seg::Entity { cols }));
            segs.push((pi, table.name.clone(), Vec::new(), Seg::Done));
            for t in db
                .schema
                .tables
                .iter()
                .filter(|t| matches!(&t.source, TableSource::Join(e, _) if *e == p.entity))
            {
                let TableSource::Join(_, field) = &t.source else {
                    continue;
                };
                let Some(i) = p.field(field) else { continue };
                segs.push((
                    pi,
                    t.name.clone(),
                    vec!["owner_id".into(), "target_id".into(), "ord".into()],
                    Seg::Join(i),
                ));
                segs.push((pi, t.name.clone(), Vec::new(), Seg::Done));
            }
            let mut traits: Vec<String> = Vec::new();
            let mut stack: Vec<String> = model
                .entity(&p.entity)
                .map(|e| e.traits.clone())
                .unwrap_or_default();
            while let Some(tr) = stack.pop() {
                if let Some(te) = model.entity(&tr) {
                    stack.extend(te.traits.iter().cloned());
                }
                traits.push(tr);
            }
            for tr in traits {
                let tname = schema::snake(&tr);
                if db
                    .schema
                    .table(&tname)
                    .is_some_and(|x| matches!(x.source, TableSource::Trait(_)))
                {
                    segs.push((
                        pi,
                        tname.clone(),
                        vec!["id".into(), "entity".into()],
                        Seg::Trait,
                    ));
                    traits_written.insert(tname);
                }
            }
        }
        // (a trait table is complete once every entity implementing it is written)
        for t in traits_written {
            segs.push((0, t, Vec::new(), Seg::Done));
        }
        Projections {
            segs,
            offsets,
            seg: 0,
            row: 0,
            created_at: mem.time.format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }

    /// The next job: up to 10 000 rows of a table, or a table complete.
    fn next_job(&mut self, mem: &Mem, prep: &Prep) -> Option<Job> {
        if self.seg == self.segs.len() {
            self.seg += 1;
            return Some(Job::ForeignKeys);
        }
        let (pi, table, cols, seg) = self.segs.get(self.seg)?;
        let p = &mem.plans[*pi];
        let start = self.row;
        let end = (start + 10_000).min(p.rows.len());
        let rows: Vec<Vec<Sql>> = match seg {
            Seg::Done => {
                self.seg += 1;
                self.row = 0;
                return Some(Job::Done(table.clone()));
            }
            Seg::Entity { cols } => (start..end)
                .map(|r| {
                    let row = &p.rows[r];
                    let mut v = Vec::with_capacity(5 + cols.len());
                    v.extend([
                        Sql::Int((p.first + r as u64) as i64),
                        Sql::Int(prep.tx_of(self.offsets[*pi] + r) as i64),
                        Sql::Text(self.created_at.clone()),
                        Sql::Null,
                        if row.frozen {
                            Sql::Int(prep.last_tx as i64)
                        } else {
                            Sql::Null
                        },
                    ]);
                    for (src, c) in cols {
                        let val = match src {
                            Some((true, i)) => row.formulas.get(*i).unwrap_or(&V::None),
                            Some((false, i)) => &row.vals[*i],
                            None => &V::None,
                        };
                        v.push(encode(c, val));
                    }
                    v
                })
                .collect(),
            Seg::Join(i) => (start..end)
                .flat_map(|r| {
                    let owner = (p.first + r as u64) as i64;
                    let ids = ref_ids(&p.rows[r].vals[*i]);
                    // (a reference given twice: its last position, as the engine's upsert keeps it)
                    let mut last: HashMap<Id, usize> = HashMap::new();
                    for (n, t) in ids.iter().enumerate() {
                        last.insert(*t, n);
                    }
                    let mut seen = BTreeSet::new();
                    ids.into_iter()
                        .filter(move |t| seen.insert(*t))
                        .map(move |t| {
                            vec![
                                Sql::Int(owner),
                                Sql::Int(t as i64),
                                Sql::Int(last[&t] as i64),
                            ]
                        })
                })
                .collect(),
            Seg::Trait => (start..end)
                .map(|r| {
                    vec![
                        Sql::Int((p.first + r as u64) as i64),
                        Sql::Text(p.entity.clone()),
                    ]
                })
                .collect(),
        };
        if end >= p.rows.len() {
            self.seg += 1;
            self.row = 0;
        } else {
            self.row = end;
        }
        Some(Job::Rows {
            table: table.clone(),
            cols: cols.clone(),
            rows,
        })
    }
}

/// The end of an import's transaction, on the connection the writer gave back: the records
/// already in the database whose formulas or warnings follow the new data, the foreign keys
/// and indexes left, the commit rows, the state; committed (or rolled back), then analyzed.
#[allow(clippy::too_many_arguments)]
fn finish_write(
    db: &mut Database,
    model: &Model,
    mem: &Mem,
    prep: &Prep,
    written: Result<Written, String>,
    existing_formulas: &HashMap<(Id, String), V>,
    existing_warnings: &HashMap<Id, Vec<String>>,
    out: &mut ImportReport,
) -> Result<(), String> {
    let sqlite = db.dialect() == Dialect::Sqlite;
    let mut analyzed: Vec<String> = Vec::new();
    let r = (|| -> Result<(), String> {
        let (history, hashes, tables, fks) = written?;
        analyzed = tables;
        out.history = history;
        if !mem.existing.is_empty() {
            let mut store = Store::default();
            for r in mem.existing.values() {
                let mut r = r.clone();
                if let Some(w) = existing_warnings.get(&r.id) {
                    r.warnings = w.clone();
                }
                store.records.insert(r.id, r);
            }
            store.materialized = existing_formulas.clone();
            let ids: Vec<Id> = store.sorted().iter().map(|r| r.id).collect();
            for id in ids {
                // their warnings or materialized formulas may change: the row changed with the
                // import's last commit
                let r = store.records[&id].clone();
                db.record_meta(&r, prep.last_tx, r.version())?;
                db.project_record(model, &store, id, &|t| t)?;
            }
        }
        if prep.rebuild_indexes {
            if !sqlite && !fks {
                // the deferred foreign keys checked now: a table with pending checks cannot be altered
                db.conn.batch("SET CONSTRAINTS ALL IMMEDIATE")?;
                let mut fk = prep.fk_dropped.clone();
                fk.dedup();
                for t in &fk {
                    if let Some(table) = db.schema.table(t).cloned() {
                        for k in table.foreign_keys() {
                            db.conn.batch(&k)?;
                        }
                    }
                }
            }
            for sql in crate::db::system_indexes_sql() {
                db.conn.batch(&sql)?;
            }
            db.sync_indexes(None, false)?;
        }
        // the commits of the chain
        let key = db.signing_key()?;
        for (tx, prev, hash) in &hashes {
            let signature = crate::signing::sign_commit(&key, hash)?;
            db.conn.exec(
                "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\", \"signature\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text, ?text)",
                &[Sql::Int(*tx as i64), Sql::Text(prep.at.clone()), Sql::Null, Sql::Text(prep.cause_text.clone()), Sql::Text(prev.clone()), Sql::Text(hash.clone()), signature],
            )?;
        }
        let next_id = mem
            .plans
            .last()
            .map(|p| p.first + p.rows.len() as u64 - 1)
            .unwrap_or(0);
        let stored_next = db
            .meta("state")?
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|s| s["next_id"].as_u64())
            .unwrap_or(0);
        db.set_meta(
            "state",
            &serde_json::json!({ "next_id": stored_next.max(next_id), "tx": prep.last_tx })
                .to_string(),
        )?;
        out.commits = hashes.iter().map(|(tx, _, _)| *tx).collect();
        Ok(())
    })();
    let r = db.finish(r);
    if sqlite {
        let _ = db.conn.batch("PRAGMA journal_mode = WAL");
        if r.is_ok() {
            // statistics for the planner: which tables are small (a row rule's sets of ids
            // start from the regions, not from every order); a sample of each index
            let _ = db.conn.batch("PRAGMA analysis_limit = 1000; ANALYZE");
        }
    } else if r.is_ok() {
        // statistics for the planner, which has none for rows loaded in bulk (it would scan
        // the history in order instead of using its indexes): the tables the import wrote
        // got theirs in its transaction (the writer), the others now
        let written: BTreeSet<String> = ["_quadra_history", "_quadra_records"]
            .iter()
            .map(|s| s.to_string())
            .chain(analyzed.iter().cloned())
            .collect();
        let list: Vec<String> = db
            .schema
            .tables
            .iter()
            .map(|t| t.name.clone())
            .filter(|t| !written.contains(t))
            .map(|t| format!("\"{t}\""))
            .collect();
        if !list.is_empty() {
            let _ = db.conn.batch(&format!("ANALYZE {}", list.join(", ")));
        }
    }
    r
}

/// The writes of an import on a thread of their own: the history from the end of the
/// defaults (while the formulas and rules are computed), the rows once they are.
struct Pipeline {
    /// PostgreSQL: the WAL written in large writes while the import runs (`WalFlusher`)
    _flusher: Option<WalFlusher>,
    history_sent: bool,
    /// the projection rows, once they can be written
    rows: Option<Projections>,
    /// which records the rows of `_quadra_records` of the late plans were written frozen
    frozen_sent: Option<Vec<bool>>,
    /// jobs of rows sent and not taken by the writer yet
    queued: Arc<AtomicUsize>,
    prep: Prep,
    feeder: Feeder,
    jobs: Option<SyncSender<Job>>,
    thread: Option<std::thread::JoinHandle<(Conn, Result<Written, String>)>>,
}

/// Entries of the history the writer may have waiting before the main thread makes more.
const HISTORY_AHEAD: usize = 1_000_000;

impl Pipeline {
    fn start(
        db: &mut Database,
        model: &Model,
        mem: &Mem,
        per_commit: usize,
    ) -> Result<Pipeline, String> {
        let mut prep = prepare(db, mem, per_commit)?;
        prep.plans = mem
            .plans
            .iter()
            .map(|p| PlanMeta {
                first: p.first,
                entity: p.entity.clone(),
                len: p.rows.len(),
                late: model.entity(&p.entity).is_some_and(|e| e.frozen.is_some()),
            })
            .collect();
        let flusher = db
            .url
            .as_deref()
            .filter(|_| db.dialect() == Dialect::Postgres)
            .and_then(WalFlusher::start);
        let conn = db.conn.take_conn().expect("the import's connection");
        let schema = (*db.schema).clone();
        let pending = Arc::new(AtomicUsize::new(0));
        let queued = Arc::new(AtomicUsize::new(0));
        let (jobs, rx) = std::sync::mpsc::sync_channel::<Job>(16);
        let wp = Prep {
            prev: prep.prev.clone(),
            at: prep.at.clone(),
            cause_text: prep.cause_text.clone(),
            cause_json: prep.cause_json.clone(),
            fk_dropped: prep.fk_dropped.clone(),
            pk_dropped: prep.pk_dropped.clone(),
            plans: prep.plans.clone(),
            ..prep
        };
        let pend = pending.clone();
        let taken = queued.clone();
        let thread = std::thread::Builder::new()
            .name("quadra-import-writer".into())
            .spawn(move || writer(conn, schema, &wp, rx, pend, taken))
            .map_err(|e| e.to_string())?;
        let feeder = Feeder::new(db, mem, pending);
        Ok(Pipeline {
            _flusher: flusher,
            history_sent: false,
            rows: None,
            frozen_sent: None,
            queued,
            prep,
            feeder,
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    /// More history for the writer when it is close to running out, more projection rows once
    /// they can be written (between two records of the evaluation: never waits).
    fn pump(&mut self, mem: &Mem) {
        if self.rows.is_some() {
            self.pump_rows(mem);
            return;
        }
        let ahead = HISTORY_AHEAD;
        if self.feeder.done || self.feeder.pending.load(Ordering::Relaxed) >= ahead / 4 {
            return;
        }
        let items = self.feeder.produce(mem, &self.prep, ahead / 4);
        if let Some(j) = &self.jobs {
            // (the channel has room: at most a few chunks are ever waiting)
            let _ = j.send(Job::History(items));
            if self.feeder.done {
                // the history is complete: the writer finishes it (its keys, its indexes)
                // while the evaluation goes on
                let _ = j.send(Job::HistoryEnd);
                self.history_sent = true;
            }
        }
    }

    /// The rest of the history, its end.
    fn end_history(&mut self, mem: &Mem) -> Result<(), String> {
        let jobs = self.jobs.as_ref().expect("the jobs");
        while !self.feeder.done {
            let items = self.feeder.produce(mem, &self.prep, HISTORY_AHEAD / 4);
            jobs.send(Job::History(items))
                .map_err(|_| "the import's writer stopped".to_string())?;
        }
        if !self.history_sent {
            jobs.send(Job::HistoryEnd)
                .map_err(|_| "the import's writer stopped".to_string())?;
            self.history_sent = true;
        }
        Ok(())
    }

    /// The projection rows can be written (the formulas and `frozen when` known): they go to
    /// the writer a chunk at a time while the rules are checked (`pump_rows`).
    fn projections(&mut self, db: &Database, model: &Model, mem: &Mem) -> Result<(), String> {
        self.end_history(mem)?;
        self.send_frozen(mem)?;
        self.rows = Some(Projections::new(db, model, mem));
        self.pump_rows(mem);
        Ok(())
    }

    /// Which records are frozen, for the records' rows of the late plans (once).
    fn send_frozen(&mut self, mem: &Mem) -> Result<(), String> {
        if self.frozen_sent.is_some() {
            return Ok(());
        }
        let frozen: Vec<bool> = mem
            .plans
            .iter()
            .flat_map(|p| p.rows.iter().map(|r| r.frozen))
            .collect();
        let jobs = self.jobs.as_ref().expect("the jobs");
        jobs.send(Job::Frozen(frozen.clone()))
            .map_err(|_| "the import's writer stopped".to_string())?;
        self.frozen_sent = Some(frozen);
        Ok(())
    }

    /// More projection rows for the writer when it has few waiting (never waits).
    fn pump_rows(&mut self, mem: &Mem) {
        let (Some(rows), Some(jobs)) = (self.rows.as_mut(), self.jobs.as_ref()) else {
            return;
        };
        while self.queued.load(Ordering::Relaxed) < 4 {
            let Some(job) = rows.next_job(mem, &self.prep) else {
                return;
            };
            self.queued.fetch_add(1, Ordering::Relaxed);
            if jobs.send(job).is_err() {
                return;
            }
        }
    }

    /// The rest: the projection rows left, the records; the connection back, with what was
    /// written.
    fn finish(&mut self, db: &mut Database, model: &Model, mem: &Mem) -> Result<Written, String> {
        let r = (|| -> Result<(), String> {
            self.end_history(mem)?;
            self.send_frozen(mem)?;
            let mut rows = self
                .rows
                .take()
                .unwrap_or_else(|| Projections::new(db, model, mem));
            let jobs = self.jobs.as_ref().expect("the jobs");
            while let Some(job) = rows.next_job(mem, &self.prep) {
                self.queued.fetch_add(1, Ordering::Relaxed);
                jobs.send(job)
                    .map_err(|_| "the import's writer stopped".to_string())?;
            }
            // the records' rows the rules changed: their warnings, `frozen when` evaluated again
            let sent = self.frozen_sent.as_deref().unwrap_or(&[]);
            let mut fix = Vec::new();
            let mut g = 0usize;
            for (p, meta) in mem.plans.iter().zip(self.prep.plans.iter()) {
                for (r, row) in p.rows.iter().enumerate() {
                    let written = meta.late && sent.get(g).copied().unwrap_or(false);
                    if written != row.frozen || !row.warnings.is_empty() {
                        let warnings =
                            serde_json::to_string(&row.warnings).unwrap_or_else(|_| "[]".into());
                        fix.push((
                            (p.first + r as u64) as i64,
                            row.frozen.then_some(self.prep.last_tx as i64),
                            warnings,
                        ));
                    }
                    g += 1;
                }
            }
            if !fix.is_empty() {
                jobs.send(Job::RecordsFix(fix))
                    .map_err(|_| "the import's writer stopped".to_string())?;
            }
            jobs.send(Job::Done("_quadra_records".into()))
                .map_err(|_| "the import's writer stopped".to_string())?;
            jobs.send(Job::Finish)
                .map_err(|_| "the import's writer stopped".to_string())?;
            Ok(())
        })();
        let jobs = self.jobs.take();
        drop(jobs);
        let (conn, written) = self.join();
        db.conn.put_conn(conn);
        // (the writer's own error first: it says why the jobs stopped)
        match (written, r) {
            (Err(e), _) if e != "cancelled" => Err(e),
            (_, Err(e)) => Err(e),
            (w, Ok(())) => w,
        }
    }

    fn join(&mut self) -> (Conn, Result<Written, String>) {
        let t = self.thread.take().expect("the writer");
        t.join()
            .unwrap_or_else(|_| panic!("the import's writer panicked"))
    }

    /// The evaluation failed: the writer stops, the transaction is rolled back.
    fn abandon(&mut self, db: &mut Database) {
        self.jobs.take();
        let (conn, _) = self.join();
        db.conn.put_conn(conn);
        let _ = db.conn.batch("ROLLBACK");
        if db.dialect() == Dialect::Sqlite {
            let _ = db.conn.batch("PRAGMA journal_mode = WAL");
        }
    }
}

/// PostgreSQL: a second connection that has the WAL written and flushed every few
/// milliseconds while an import runs. The import's backend fills the WAL buffers (4 MB with
/// the default `wal_buffers`) far faster than the WAL writer empties them (it wakes every
/// `wal_writer_delay`, 200 ms): the backend then writes each page it needs back itself, 8 kB
/// at a time, and with `wal_sync_method = open_datasync` (the default on macOS and Windows)
/// each of those writes waits for the disk — 70% of the import's time in the database. A
/// commit writes everything before it in a few large writes: the flusher commits an empty
/// transaction in a loop (an xid, nothing written). Only where the writes are synchronous (an
/// `open_*` method); it changes no data, and is not started if its connection cannot be opened.
struct WalFlusher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WalFlusher {
    fn start(url: &str) -> Option<WalFlusher> {
        let Ok(Conn::Postgres(mut c)) = Conn::open(url) else {
            return None;
        };
        let method: String = c.query_one("SHOW wal_sync_method", &[]).ok()?.get(0);
        if !method.starts_with("open_") {
            return None;
        }
        // (flushed locally: a synchronous standby is not waited for)
        c.batch_execute("SET synchronous_commit = local").ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("quadra-import-wal".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    if c.batch_execute("SELECT pg_current_xact_id()").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
            .ok()?;
        Some(WalFlusher {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for WalFlusher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- CSV ------------------------------------------------------------------------------------

/// Read `<Entity>.csv` files of a directory into an import, entities referenced by others
/// first. Returns the entities found.
pub fn read_dir(import: &mut Import, dir: &Path) -> Result<Vec<String>, String> {
    let mut found: Vec<(String, std::path::PathBuf)> = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("csv") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if import.model.entity(stem).is_none() {
            return Err(format!("{}: no entity `{stem}` in the model (files are named after their entity: `Customer.csv`)", path.display()));
        }
        found.push((stem.to_string(), path));
    }
    if found.is_empty() {
        return Err(format!("no `<Entity>.csv` files in {}", dir.display()));
    }
    found.sort();
    let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
    for name in order(import.model, import.report, &names) {
        let path = &found.iter().find(|(n, _)| *n == name).unwrap().1;
        read_csv(import, &name, path)?;
    }
    Ok(names)
}

/// Entities referenced by others first (alphabetical among equals; cycles keep that order).
pub fn order(model: &Model, report: &Report, names: &[String]) -> Vec<String> {
    let deps = |n: &str| -> HashSet<String> {
        let Some(e) = model.entity(n) else {
            return HashSet::new();
        };
        model
            .all_fields(e)
            .into_iter()
            .filter(|f| f.formula.is_none() && !schema::is_inverse(model, e, f))
            .filter_map(|f| {
                report
                    .field_types
                    .get(&(e.name.clone(), f.name.clone()))
                    .and_then(|t| kind_of(model, t))
                    .and_then(|k| k.target().map(str::to_string))
            })
            .flat_map(|t| match model.entity(&t) {
                Some(te) if te.kind == EntityKind::Trait => model
                    .implementors(&t)
                    .iter()
                    .map(|x| x.name.clone())
                    .collect::<Vec<_>>(),
                _ => vec![t],
            })
            .filter(|t| t != n && names.contains(t))
            .collect()
    };
    let mut left: Vec<String> = names.to_vec();
    let mut out = Vec::new();
    while !left.is_empty() {
        let next = left
            .iter()
            .position(|n| deps(n).iter().all(|d| out.contains(d)))
            .unwrap_or(0);
        out.push(left.remove(next));
    }
    out
}

fn read_csv(import: &mut Import, entity: &str, path: &Path) -> Result<(), String> {
    use std::io::BufRead;
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let shown = path.display().to_string();
    let mut lines = std::io::BufReader::with_capacity(1 << 20, file).lines();
    let mut handle = None;
    let mut n = 0usize;
    let mut cells: Vec<String> = Vec::new();
    while let Some(line) = lines.next() {
        let mut line = line.map_err(|e| format!("{shown}: {e}"))?;
        n += 1;
        if handle.is_some() && (line.trim().is_empty() || line.trim_start().starts_with('#')) {
            continue;
        }
        let at = n;
        // a quoted cell may hold line breaks
        while line.matches('"').count() % 2 == 1 {
            match lines.next() {
                Some(more) => {
                    line.push('\n');
                    line.push_str(&more.map_err(|e| format!("{shown}: {e}"))?);
                    n += 1;
                }
                None => return Err(format!("{shown} line {at}: a quote is not closed")),
            }
        }
        split_csv(&line, &mut cells);
        match handle {
            None => handle = Some(import.entity(entity, &cells, &shown)?),
            Some(h) => {
                let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
                import.row(h, &refs, at)?;
            }
        }
    }
    if handle.is_none() {
        return Err(format!("{shown}: no header line"));
    }
    Ok(())
}

/// One CSV line: `,` between cells, `"…"` quotes a cell, `""` is a quote inside it.
fn split_csv(line: &str, out: &mut Vec<String>) {
    out.clear();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            (c, _) => cur.push(c),
        }
    }
    out.push(cur);
}

/// A cell for a CSV file: quoted when it holds a separator, a quote or a line break.
pub fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// `quadra db import <dir>`: open, read the files, validate, write.
pub fn import_dir(
    url: &str,
    model: &Model,
    report: &Report,
    dir: &Path,
    time: NaiveDateTime,
    per_commit: Option<usize>,
    progress: &mut dyn FnMut(&str),
) -> Result<ImportReport, String> {
    let mut db = Database::open(url, model, report)?;
    let mut import = Import::new(&mut db, model, report, time, &dir.display().to_string())?;
    if let Some(n) = per_commit {
        import.records_per_commit = n;
    }
    read_dir(&mut import, dir)?;
    import.run(db, progress)
}
