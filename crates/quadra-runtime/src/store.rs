//! The engine's working set: records with full history, ledger rows, counters.
//!
//! Without a [`Source`] the store holds everything (tests, batch jobs). With a source
//! (a database) it starts empty and loads on demand, as of the source's snapshot
//! transaction: a record when it is read, the ids of an entity when it is scanned, ledger
//! rows by key when they are summed. It also records what was read (the read set), so a
//! commit can be refused when something it relied on changed in the meantime.
//!
//! A transaction is a clone of the working set: on error the clone is restored.

use crate::value::{Id, V};
use chrono::NaiveDateTime;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

/// Why something happened: an action, a reaction, a test statement… with its parent cause.
#[derive(Debug)]
pub struct Cause {
    pub desc: String,
    pub user: String,
    pub time: NaiveDateTime,
    pub parent: Option<Rc<Cause>>,
}

impl Cause {
    pub fn chain(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{}  ({}, {})",
            self.desc,
            self.user,
            self.time.format("%Y-%m-%d %H:%M")
        )];
        let mut cur = self.parent.clone();
        while let Some(c) = cur {
            if c.parent.is_none() && c.desc == "setup" {
                break;
            }
            out.push(format!(
                "{}  ({}, {})",
                c.desc,
                c.user,
                c.time.format("%Y-%m-%d %H:%M")
            ));
            cur = c.parent.clone();
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    pub ent: Rc<str>,
    pub id: Id,
    /// Stored values. Per-company fields use the key `field@<company id>`.
    pub fields: rustc_hash::FxHashMap<String, V>,
    /// (transaction, field, new value): the complete history.
    pub history: Vec<(u64, String, V)>,
    pub created_tx: u64,
    pub created_at: NaiveDateTime,
    pub created_by: Option<Id>,
    /// Fields whose `default` has not been computed yet.
    pub pending_defaults: Vec<String>,
    /// Transaction after which the record is frozen (it then reads the world as of then).
    pub frozen_at: Option<u64>,
    pub cause: Rc<Cause>,
    pub warnings: Vec<String>,
    /// Loaded from its projection row: `history` then holds one entry per field at this
    /// transaction (the last one that wrote the record, `_quadra_records.version_tx`), not the
    /// real history, which `Store::read` loads when a read as of an earlier transaction needs
    /// it. None: `history` is the whole history (up to the snapshot).
    pub partial: Option<u64>,
    /// The values of the incremental formulas (`quadra_analysis::deps::incremental`) as the
    /// database holds them at the snapshot: read instead of computing the formula while
    /// nothing it depends on was written (`Interp::get`).
    pub formulas: HashMap<String, V>,
}

impl Record {
    /// Last transaction that wrote the record.
    pub fn version(&self) -> u64 {
        self.history
            .iter()
            .map(|(t, _, _)| *t)
            .max()
            .unwrap_or(self.created_tx)
            .max(self.created_tx)
    }

    /// The value of a stored field before transaction `tx` (None: no value, or created in it).
    pub fn before(&self, tx: u64, key: &str) -> Option<V> {
        if self.created_tx >= tx {
            return None;
        }
        self.history
            .iter()
            .rev()
            .find(|(t, f, _)| *t < tx && f == key)
            .map(|(_, _, v)| v.clone())
    }
}

#[derive(Clone, Debug)]
pub struct LedgerRow {
    pub ledger: Rc<str>,
    pub id: Id,
    pub cols: HashMap<String, V>,
    pub source: Option<V>,
    pub date: NaiveDateTime,
    pub tx: u64,
}

/// Committed data, read as of a snapshot transaction (`base`).
pub trait Source {
    /// The snapshot: every read sees the database as it was after this transaction.
    fn base(&self) -> u64;
    /// A record with its history up to `base`; None when it does not exist at `base`.
    fn record(&mut self, id: Id) -> Option<Record>;
    /// Ids of the records of an entity (exact name) existing at `base`.
    fn ids(&mut self, ent: &str) -> Vec<Id>;
    /// Several records at once (those that exist at `base`).
    fn records(&mut self, ids: &[Id]) -> Vec<Record> {
        ids.iter().filter_map(|id| self.record(*id)).collect()
    }
    /// `record` of a record expected to be of entity `ent` (a hint: a reference's entity, the
    /// entity a query read; the record may be of another entity).
    fn record_of(&mut self, _ent: &str, id: Id) -> Option<Record> {
        self.record(id)
    }
    /// `records` of records expected to be of entity `ent` (`record_of`).
    fn records_of(&mut self, _ent: &str, ids: &[Id]) -> Vec<Record> {
        self.records(ids)
    }
    /// Whether `record_of` makes use of its hint (the store then keeps the hints).
    fn wants_hints(&self) -> bool {
        false
    }
    /// A record with its whole history up to `base` (a record first loaded from its projection
    /// row read as of an earlier transaction): None when the source has no other way.
    fn full_record(&mut self, id: Id) -> Option<Record> {
        self.record(id).filter(|r| r.partial.is_none())
    }
    /// Ids of records of `ent` whose stored `field` equals `value` (an index lookup).
    /// None when the field cannot be looked up this way: the caller scans.
    fn find(&mut self, ent: &str, field: &str, value: &V) -> Option<Vec<Id>>;
    /// `find` of several records at once (the lines of a page of records): referenced id →
    /// ids of the records of `ent` whose `field` holds it. None: one `find` each.
    fn find_many(
        &mut self,
        _ent: &str,
        _field: &str,
        _targets: &[Id],
    ) -> Option<HashMap<Id, Vec<Id>>> {
        None
    }
    /// Rows of a ledger, all or those whose `col` equals a value.
    fn rows(&mut self, ledger: &str, filter: Option<(&str, &V)>) -> Vec<LedgerRow>;
    fn row(&mut self, id: Id) -> Option<LedgerRow>;
    /// Postings in the ledgers of a record: ledger key → rows.
    fn postings(&mut self, record: Id) -> HashMap<String, Vec<HashMap<String, V>>>;
    fn counter(&mut self, key: &str) -> u64;
    /// When and by whom a committed transaction was made.
    fn commit(&mut self, _tx: u64) -> Option<(NaiveDateTime, Option<Id>)> {
        None
    }
    /// Reserve `n` record ids for this process; returns the first.
    fn reserve_ids(&mut self, n: u64) -> Id;
    /// The first infrastructure error met while loading, if any.
    fn error(&mut self) -> Option<String>;
    /// A migration renames `old` to `new` in `ent` at transaction `tx`: records loaded from
    /// now on read their earlier history under the new name. Returns how many committed
    /// values (field keys of records) the rename moves.
    fn rename(&mut self, _ent: &str, _old: &str, _new: &str, _tx: u64) -> usize {
        0
    }
    /// A query over a whole entity, in SQL (`crate::query`). An error when the source has no
    /// SQL or cannot translate what the query needs (the order): the interpreter does it.
    fn select(&mut self, _r: &crate::query::Request) -> Result<crate::query::Answer, String> {
        Err("no SQL".into())
    }
    /// Whether records of these entities were written after `base` (what SQL reads is the
    /// latest data, not the snapshot): the interpreter then answers the query itself.
    fn changed_since(&mut self, _entities: &[String]) -> Result<bool, String> {
        Ok(true)
    }
    /// Records of `ent` one of whose text `fields` starts with `q` (already in ASCII lower
    /// case), then those where it contains it, each group by the record's display text (lower
    /// case) and id: at most `limit` rows after the cursor, with the cursor of each row.
    /// `perm`: only the rows the user may read (in SQL when the rules translate: then each row
    /// says whether the interpreter must decide it; otherwise every row does).
    /// `words`: the second group is the records matching this full-text search instead of
    /// those containing the text.
    #[allow(clippy::too_many_arguments)]
    fn search(
        &mut self,
        _ent: &str,
        _fields: &[String],
        _q: &str,
        _words: Option<&crate::query::TextSearch>,
        _after: Option<&crate::query::After>,
        _limit: usize,
        _perm: Option<(&quadra_ir::Model, &crate::query::Perm, &crate::query::Frees)>,
    ) -> Result<crate::query::Searched, String> {
        Err("no SQL".into())
    }
}

pub type SharedSource = Rc<RefCell<dyn Source>>;

/// What a transaction relied on: checked at commit against what others committed since.
#[derive(Clone, Debug, Default)]
pub struct ReadSet {
    pub records: BTreeSet<Id>,
    /// entities whose full list of ids was read
    pub scans: BTreeSet<String>,
    /// (entity, field) looked up by value
    pub finds: BTreeSet<(String, String)>,
    /// (ledger, filter column, filter value as text); no filter = the whole ledger
    pub ledgers: BTreeSet<(String, Option<(String, String)>)>,
    /// counters with the value seen
    pub counters: BTreeMap<String, u64>,
    /// records whose materialized formulas were read from the database (not computed): their
    /// rows must not have changed since (a formula's value is not in the history)
    pub formulas: BTreeSet<Id>,
    /// entities whose materialized formula columns a query read in SQL
    pub formula_scans: BTreeSet<String>,
    /// the same reads as `formulas`, by field: the sequencer tells a change of another
    /// formula of the record from one of these (the database's check knows only records)
    pub formula_fields: BTreeSet<(Id, String)>,
    /// (entity, field, value as `value_keys` says it) looked up by value: only a write of that
    /// value (to it, or from it) changes the answer. The database's check takes them by
    /// (entity, field), as `finds`.
    pub find_values: BTreeSet<(String, String, String)>,
}

impl ReadSet {
    /// The stored value of formula `field` of record `id` was read.
    pub fn formula(&mut self, id: Id, field: &str) {
        self.formulas.insert(id);
        self.formula_fields.insert((id, field.to_string()));
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
            && self.find_values.is_empty()
            && self.scans.is_empty()
            && self.finds.is_empty()
            && self.ledgers.is_empty()
            && self.counters.is_empty()
            && self.formulas.is_empty()
            && self.formula_scans.is_empty()
    }
}

/// A record's values erased in the current transaction (`erase`, a retention that ended): the
/// commit takes their past values out of the history, keeping their digests. `whole`: the
/// record itself is gone (its entity's `expire`), with all its fields.
#[derive(Clone, Debug)]
pub struct Erasure {
    pub id: Id,
    pub ent: Rc<str>,
    pub fields: Vec<String>,
    pub whole: bool,
    /// why: `erase`'s reason, or the retention that ended
    pub reason: String,
    pub by: Option<Id>,
    /// `history 1 year` of the entity: the values of all its records replaced before this
    /// instant leave the history (`id` 0, no fields); the values in force stay
    pub before: Option<chrono::NaiveDateTime>,
    /// `history last N` (or `none`: 1) of the entity: the versions of each value of all its
    /// records beyond the last N leave the history (`id` 0, no fields)
    pub last: Option<usize>,
}

#[derive(Clone, Default)]
pub struct Store {
    /// (a hash map: a record is looked up on every read of one of its fields; code that goes
    /// through every record sorts them by id when the order matters)
    pub records: rustc_hash::FxHashMap<Id, Record>,
    /// Ids known per entity; complete only for the entities in `complete` (or without a source).
    pub by_entity: HashMap<String, BTreeSet<Id>>,
    pub rows: Vec<LedgerRow>,
    /// Postings currently in the ledger for each (source record, ledger).
    pub postings: HashMap<(Id, String), Vec<HashMap<String, V>>>,
    pub next_id: Id,
    pub tx: u64,
    /// The committed transaction this working set was read at: a commit is checked against
    /// what others committed after it.
    pub base: u64,
    pub counters: HashMap<String, u64>,
    /// When and by whom each known transaction was made (`updated_at`, `updated_by`).
    pub commits: HashMap<u64, (NaiveDateTime, Option<Id>)>,
    /// Values written so far (to tell whether anything changed since a point).
    pub writes: u64,
    /// Events emitted in the current transaction (`emit`): written to the database's outbox
    /// with the commit, dropped with a rollback.
    pub outbox: Vec<V>,
    /// Ledger rows posted by records in the current transaction (not the automatic
    /// reversals): the ledger's plain `check`s hold on each of them.
    pub posted: Vec<Id>,
    /// Values and records erased in the current transaction (crate::interp::retention)
    pub erasures: Vec<Erasure>,
    /// The rows each (record, post) posted in the current transaction.
    pub posted_rows: HashMap<(Id, String), Vec<Id>>,
    /// `becomes` reactions already fired in this transaction, and the last known value
    /// of their condition per record.
    pub fired: HashSet<(usize, Id)>,
    pub becomes: HashMap<(usize, Id), bool>,
    /// Last value written to the database for each materialized formula.
    pub materialized: HashMap<(Id, String), V>,
    /// Migrations and changes already run: `migration:<name>`, `change:<name>`,
    /// `revert:<name>` → transaction.
    pub applied: BTreeMap<String, u64>,
    /// Field renames: (entity, old name, new name, transaction). History entries written
    /// before the transaction under the old name are read under the new one.
    pub renames: Vec<(String, String, String, u64)>,
    /// entity → how many versions of a value written before the transaction its records keep
    /// in the working set (`history none`: 1, `history last N`: N; not there: all): the
    /// history of the database keeps no more (`Interp::sync_keep`)
    pub keep: std::rc::Rc<rustc_hash::FxHashMap<String, usize>>,
    /// Records created or written in the current transaction.
    pub dirty: BTreeSet<Id>,
    /// Records unfrozen in the current transaction (`frozen when` stopped holding).
    pub thawed: BTreeSet<Id>,
    /// (user, key, request hash, response): an API request committed with this transaction
    pub idempotency: Option<(Id, String, String, String)>,
    /// Rollups this transaction changed by delta while committing: (record, field) → (value
    /// stored before, value now). Written as an addition to whatever value the database holds
    /// then, so they commute with other transactions' (no read of the stored value).
    pub deltas: BTreeMap<(Id, String), (V, V)>,
    /// Rules on such rollups (`total <= limit`): held again on the value the database holds
    /// when the commit is written.
    pub guards: Vec<Guard>,

    // ---- loading on demand ----
    /// What a record loaded from the source was when loaded (frozen at, warnings, cause): a
    /// commit writes the metadata and projection of a record it did not create only when one
    /// of them, or a field, changed
    pub origin: HashMap<Id, (Option<u64>, Vec<String>, Rc<Cause>)>,
    /// committed candidates of `find` looked up in advance (`prefetch_find`): (entity, field,
    /// referenced id) → ids
    found: HashMap<(String, String, Id), Vec<Id>>,
    pub src: Option<SharedSource>,
    pub reads: ReadSet,
    complete: HashSet<String>,
    absent: HashSet<Id>,
    row_ids: HashSet<Id>,
    rows_loaded: HashSet<String>,
    postings_loaded: HashSet<Id>,
    counters_loaded: HashSet<String>,
    /// reserved ids: next, end (exclusive)
    id_block: (Id, Id),
    /// how many ids to reserve at a time (0 = 100)
    pub id_block_size: u64,
    /// Records whose field was written in this working set, per (entity, field): `find` adds
    /// those that match now to the source's committed candidates.
    written_by: HashMap<(Rc<str>, String), BTreeSet<Id>>,
    /// In memory (no source): (entity, field) → referenced id → records whose field holds
    /// it, built the first time an inverse relation is followed and kept up to date by `write`.
    ref_index: HashMap<(Rc<str>, String), HashMap<Id, BTreeSet<Id>>>,
    /// Defaults that records loaded from the source get when they lack the field (a deploy
    /// adding fields with a default): entity → fields.
    pub load_defaults: HashMap<String, Vec<String>>,
    /// Nothing will be committed from this working set (a bulk import, a deploy going through
    /// the records): what is read is not recorded for the conflict check.
    pub untracked: bool,
    /// The writes of the current transaction in order: (record, field), `""` for a record
    /// created. The incremental formulas follow it to know which of their values went stale
    /// (`Interp::stale`); `wlog_gen` changes when it starts again.
    pub wlog: Vec<(Id, Rc<str>)>,
    pub wlog_gen: u64,
    /// Which working set this is (0 in memory): what the interpreter keeps about the
    /// working set (values of formulas computed) is for this one only.
    pub uid: u64,
    /// Changes whenever a value a formula may read changes (a write, a record created, frozen
    /// or unfrozen): the values of formulas computed since are valid while it stays the same.
    pub epoch: u64,
    /// The entity records not loaded yet are expected to be of (the entity of a reference to
    /// them, of a query that found them): the source reads their rows with them
    /// (`Source::records_of`).
    hints: HashMap<Id, Rc<str>>,
    /// the source makes use of hints (`Source::wants_hints`)
    hinting: bool,
    /// Values given to the stored formulas of records when they are loaded, instead of those
    /// of their rows (`quadra db verify`: the values computed for rows found wrong).
    pub formula_overrides: Rc<HashMap<Id, Vec<(String, V)>>>,
}

/// A rule on a rollup changed by delta: `field OP bound` must hold on the record's latest value.
#[derive(Clone, Debug)]
pub struct Guard {
    pub id: Id,
    pub field: String,
    pub op: quadra_ir::BinOp,
    pub bound: V,
}

/// The change from `old` to `new` of a value that can be added to (an integer, a decimal, an
/// amount of money in the same currency, a quantity in the same unit).
pub fn delta_amount(old: &V, new: &V) -> Option<rust_decimal::Decimal> {
    match (old, new) {
        (V::Int(a), V::Int(b)) => {
            Some(rust_decimal::Decimal::from(*b) - rust_decimal::Decimal::from(*a))
        }
        (V::Dec(a), V::Dec(b)) => Some(*b - *a),
        (V::Money { amt: a, cur: x }, V::Money { amt: b, cur: y }) if x == y => Some(*b - *a),
        (V::Qty { v: a, unit: x }, V::Qty { v: b, unit: y }) if x == y => Some(*b - *a),
        _ => None,
    }
}

/// The amount of a value `delta_amount` adds to, and what it is in (the currency, the unit).
pub fn amount_of(v: &V) -> Option<(rust_decimal::Decimal, String)> {
    match v {
        V::Int(i) => Some(((*i).into(), String::new())),
        V::Dec(d) => Some((*d, String::new())),
        V::Money { amt, cur } => Some((*amt, cur.to_string())),
        V::Qty { v, unit } => Some((*v, unit.to_string())),
        _ => None,
    }
}

/// Equality for lookups: references compare by id, a set matches when it contains the value.
pub fn same(stored: Option<&V>, value: &V) -> bool {
    match (stored, value) {
        (Some(V::Ref { id: a, .. }), V::Ref { id: b, .. }) => a == b,
        (Some(V::Set(items)) | Some(V::List(items)), V::Ref { id: b, .. }) => {
            items.iter().any(|i| i.as_ref_id() == Some(*b))
        }
        (Some(x), y) => x == y,
        (None, _) => false,
    }
}

/// The ids a stored value refers to: a reference, or the references of a set or list.
pub fn ref_ids(v: &V) -> Vec<Id> {
    match v {
        V::Ref { id, .. } => vec![*id],
        V::Set(items) | V::List(items) => items.iter().filter_map(|i| i.as_ref_id()).collect(),
        _ => Vec::new(),
    }
}

/// The keys of a value as lookups by value see it: a reference by its id, a set or a list by
/// each of its items, nothing for no value.
pub fn value_keys(v: &V) -> Vec<String> {
    match v {
        V::None => Vec::new(),
        V::Set(items) | V::List(items) => items.iter().map(json_key).collect(),
        other => vec![json_key(other)],
    }
}

fn json_key(v: &V) -> String {
    match v {
        V::Ref { id, .. } => format!("#{id}"),
        other => other.to_string(),
    }
}

impl Store {
    /// The records of the working set in id order.
    pub fn sorted(&self) -> Vec<&Record> {
        let mut v: Vec<&Record> = self.records.values().collect();
        v.sort_unstable_by_key(|r| r.id);
        v
    }

    /// An empty working set over committed data.
    pub fn over(src: SharedSource) -> Store {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let (base, hinting) = {
            let s = src.borrow();
            (s.base(), s.wants_hints())
        };
        let uid = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Store {
            tx: base,
            base,
            src: Some(src),
            uid,
            hinting,
            ..Default::default()
        }
    }

    pub fn lazy(&self) -> bool {
        self.src.is_some()
    }

    pub fn new_id(&mut self) -> Id {
        if let Some(src) = &self.src {
            if self.id_block.0 >= self.id_block.1 {
                let n = if self.id_block_size == 0 {
                    100
                } else {
                    self.id_block_size
                };
                let first = src.borrow_mut().reserve_ids(n);
                self.id_block = (first, first + n);
            }
            let id = self.id_block.0;
            self.id_block.0 += 1;
            self.next_id = self.next_id.max(id);
            return id;
        }
        self.next_id += 1;
        self.next_id
    }

    pub fn id_block(&self) -> (Id, Id) {
        self.id_block
    }

    pub fn set_id_block(&mut self, b: (Id, Id)) {
        self.id_block = b;
    }

    pub fn insert(&mut self, r: Record) {
        self.by_entity
            .entry(r.ent.to_string())
            .or_default()
            .insert(r.id);
        self.dirty.insert(r.id);
        self.absent.remove(&r.id);
        self.epoch += 1;
        if !self.untracked {
            self.wlog.push((r.id, Rc::from("")));
        }
        for (k, v) in &r.fields {
            self.note_written(&r.ent, r.id, k, None, Some(v));
        }
        self.records.insert(r.id, r);
    }

    /// Keep the lookup structures current after `field` of a record changed.
    fn note_written(
        &mut self,
        ent: &Rc<str>,
        id: Id,
        field: &str,
        old: Option<&V>,
        new: Option<&V>,
    ) {
        if self.src.is_some() {
            self.written_by
                .entry((ent.clone(), field.to_string()))
                .or_default()
                .insert(id);
        }
        if let Some(idx) = self.ref_index.get_mut(&(ent.clone(), field.to_string())) {
            for t in old.map(ref_ids).unwrap_or_default() {
                if let Some(s) = idx.get_mut(&t) {
                    s.remove(&id);
                }
            }
            for t in new.map(ref_ids).unwrap_or_default() {
                idx.entry(t).or_default().insert(id);
            }
        }
    }

    /// A record loaded from the source: known for its entity, with the defaults a deploy
    /// gives the fields it lacks.
    fn loaded(&mut self, r: Record) {
        let id = r.id;
        self.loaded_quiet(r);
        if !self.untracked {
            self.reads.records.insert(id);
        }
    }

    /// `loaded` without recording a read (loaded in advance: the read is recorded when it
    /// happens, `load`).
    fn loaded_quiet(&mut self, mut r: Record) {
        self.by_entity
            .entry(r.ent.to_string())
            .or_default()
            .insert(r.id);
        if let Some(over) = self.formula_overrides.get(&r.id) {
            for (f, v) in over {
                r.formulas.insert(f.clone(), v.clone());
            }
        }
        // what it refers to is of the entity its reference says
        if self.hinting {
            self.hints.remove(&r.id);
        }
        let hinting = self.hinting;
        for v in r.fields.values().filter(|_| hinting) {
            match v {
                V::Ref { ent, id } => self.hint(*id, ent),
                V::Set(items) | V::List(items) => {
                    for it in items.iter() {
                        if let V::Ref { ent, id } = it {
                            self.hint(*id, ent);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(fields) = self.load_defaults.get(&*r.ent) {
            for f in fields {
                if !r.fields.contains_key(f) && !r.pending_defaults.contains(f) {
                    r.pending_defaults.push(f.clone());
                }
            }
        }
        self.origin
            .insert(r.id, (r.frozen_at, r.warnings.clone(), r.cause.clone()));
        self.records.insert(r.id, r);
    }

    /// Records of `ent` whose `field` refers to `target`, in memory (no source): an index
    /// built on first use. Callers still read the field (the index may list a record that
    /// was removed since).
    pub fn referencing_mem(&mut self, ent: &str, field: &str, target: Id) -> Vec<Id> {
        let key: (Rc<str>, String) = (Rc::from(ent), field.to_string());
        if !self.ref_index.contains_key(&key) {
            let mut idx: HashMap<Id, BTreeSet<Id>> = HashMap::new();
            for id in self.by_entity.get(ent).into_iter().flatten() {
                if let Some(v) = self.records.get(id).and_then(|r| r.fields.get(field)) {
                    for t in ref_ids(v) {
                        idx.entry(t).or_default().insert(*id);
                    }
                }
            }
            self.ref_index.insert(key.clone(), idx);
        }
        self.ref_index[&key]
            .get(&target)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The lookup structures of an entity no longer match its records (fields renamed).
    pub fn forget_indexes(&mut self, ent: &str) {
        self.ref_index.retain(|(e, _), _| &**e != ent);
    }

    /// A field renamed in the records of the working set: what `find` knows follows it.
    pub fn renamed_field(&mut self, ent: &str, old: &str, new: &str) {
        self.forget_indexes(ent);
        let moved: Vec<((Rc<str>, String), BTreeSet<Id>)> = self
            .written_by
            .iter()
            .filter(|((e, f), _)| &**e == ent && f == old)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for ((e, _), ids) in moved {
            self.written_by
                .entry((e, new.to_string()))
                .or_default()
                .extend(ids);
        }
    }

    /// Drop from the working set every record `keep` does not keep (a deploy goes through
    /// the records batch by batch). What was read is forgotten: nothing is committed from here.
    pub fn evict(&mut self, keep: &dyn Fn(Id) -> bool) {
        self.records.retain(|id, _| keep(*id));
        self.origin.retain(|id, _| keep(*id));
        for ids in self.by_entity.values_mut() {
            ids.retain(|id| keep(*id));
        }
        self.complete.clear();
        self.absent.clear();
        self.hints.clear();
        self.materialized.retain(|(id, _), _| keep(*id));
        self.postings.retain(|(id, _), _| keep(*id));
        self.postings_loaded.retain(|id| keep(*id));
        self.fired.retain(|(_, id)| keep(*id));
        self.becomes.retain(|(_, id), _| keep(*id));
        for ids in self.written_by.values_mut() {
            ids.retain(|id| keep(*id));
        }
        self.reads = ReadSet::default();
    }

    /// Forget loaded records nobody wrote (a loop going through a large query keeps its memory
    /// bounded): they are loaded again when read. What was read stays in the read set.
    pub fn release(&mut self, ids: &[Id]) {
        if self.src.is_none() {
            return;
        }
        for id in ids {
            if self.dirty.contains(id)
                || self
                    .records
                    .get(id)
                    .map_or(true, |r| !r.pending_defaults.is_empty())
            {
                continue;
            }
            self.records.remove(id);
        }
    }

    /// Expect a record not loaded yet to be of entity `ent` (see `hints`).
    pub fn hint(&mut self, id: Id, ent: &Rc<str>) {
        if !self.hinting || self.records.contains_key(&id) {
            return;
        }
        if self.hints.len() >= 200_000 {
            self.hints.clear();
        }
        self.hints.entry(id).or_insert_with(|| ent.clone());
    }

    /// `load` of a record expected to be of entity `ent` (a hint for the source).
    pub fn load_as(&mut self, id: Id, ent: &str) -> bool {
        if self.hinting && !self.records.contains_key(&id) && !self.hints.contains_key(&id) {
            self.hints.insert(id, Rc::from(ent));
        }
        self.load(id)
    }

    /// `prefetch` of records expected to be of entity `ent`.
    pub fn prefetch_as(&mut self, ent: &str, ids: &[Id]) {
        if self.hinting {
            let e: Rc<str> = Rc::from(ent);
            for id in ids {
                self.hint(*id, &e);
            }
        }
        self.prefetch(ids);
    }

    /// Records from the source, those with a hint by entity.
    fn fetch(&mut self, src: &SharedSource, missing: &[Id]) -> Vec<Record> {
        let mut groups: BTreeMap<Option<Rc<str>>, Vec<Id>> = BTreeMap::new();
        for id in missing {
            groups
                .entry(self.hints.get(id).cloned())
                .or_default()
                .push(*id);
        }
        let mut out = Vec::new();
        let mut src = src.borrow_mut();
        for (hint, ids) in groups {
            match hint {
                Some(ent) => out.extend(src.records_of(&ent, &ids)),
                None => out.extend(src.records(&ids)),
            }
        }
        out
    }

    /// Make sure a record is in the working set (loading it if needed).
    pub fn load(&mut self, id: Id) -> bool {
        if self.records.contains_key(&id) {
            if self.src.is_some() && !self.untracked {
                self.reads.records.insert(id);
            }
            return true;
        }
        let Some(src) = self.src.clone() else {
            return false;
        };
        if self.absent.contains(&id) {
            return false;
        }
        let rec = match self.hints.get(&id).cloned() {
            Some(ent) => src.borrow_mut().record_of(&ent, id),
            None => src.borrow_mut().record(id),
        };
        match rec {
            Some(r) => {
                self.loaded(r);
                true
            }
            None => {
                self.absent.insert(id);
                false
            }
        }
    }

    /// Load several records in one go (a scan about to read them all).
    pub fn prefetch(&mut self, ids: &[Id]) {
        let Some(src) = self.src.clone() else { return };
        let missing: Vec<Id> = ids
            .iter()
            .copied()
            .filter(|id| !self.records.contains_key(id) && !self.absent.contains(id))
            .collect();
        if missing.is_empty() {
            return;
        }
        let found = self.fetch(&src, &missing);
        let mut seen = HashSet::new();
        for r in found {
            seen.insert(r.id);
            self.loaded(r);
        }
        for id in missing {
            if !seen.contains(&id) {
                self.absent.insert(id);
            }
        }
    }

    /// Load the records that `ids` refer to (their reference fields), and theirs, `depth`
    /// levels: rules about to read paths from them (`line.product.category.active`) then find
    /// them loaded, in one query per level.
    pub fn prefetch_refs(&mut self, ids: &[Id], depth: usize) {
        if self.src.is_none() {
            return;
        }
        let mut level: Vec<Id> = ids.to_vec();
        for _ in 0..depth {
            self.prefetch_quiet(&level);
            let mut next: Vec<Id> = Vec::new();
            for id in &level {
                if let Some(r) = self.records.get(id) {
                    for v in r.fields.values() {
                        if let V::Ref { id: t, .. } = v {
                            if !self.records.contains_key(t) && !self.absent.contains(t) {
                                next.push(*t);
                            }
                        }
                    }
                }
            }
            next.sort();
            next.dedup();
            if next.is_empty() {
                return;
            }
            level = next;
        }
        self.prefetch_quiet(&level);
    }

    /// `prefetch` without recording reads (see `loaded_quiet`).
    fn prefetch_quiet(&mut self, ids: &[Id]) {
        let Some(src) = self.src.clone() else { return };
        let missing: Vec<Id> = ids
            .iter()
            .copied()
            .filter(|id| !self.records.contains_key(id) && !self.absent.contains(id))
            .collect();
        if missing.is_empty() {
            return;
        }
        let found = self.fetch(&src, &missing);
        let mut seen = HashSet::new();
        for r in found {
            seen.insert(r.id);
            self.loaded_quiet(r);
        }
        for id in missing {
            if !seen.contains(&id) {
                self.absent.insert(id);
            }
        }
    }

    pub fn get(&mut self, id: Id) -> Option<&Record> {
        self.load(id);
        self.records.get(&id)
    }

    pub fn get_mut(&mut self, id: Id) -> Option<&mut Record> {
        self.load(id);
        self.records.get_mut(&id)
    }

    /// Value of a stored field as of a transaction (None = now).
    pub fn read(&mut self, id: Id, key: &str, as_of: Option<u64>) -> Option<V> {
        self.load(id);
        if let Some(t) = as_of {
            self.history_from(id, t);
        }
        self.peek(id, key, as_of)
    }

    /// The transaction a record's history starts from in the working set (0 when it is whole).
    pub fn history_start(&self, id: Id) -> u64 {
        self.records.get(&id).and_then(|r| r.partial).unwrap_or(0)
    }

    /// Make sure the history of a record loaded from its projection row reaches back to `t`:
    /// otherwise its whole history is loaded (the writes of this working set kept on top).
    pub fn history_from(&mut self, id: Id, t: u64) {
        let Some(v) = self.records.get(&id).and_then(|r| r.partial) else {
            return;
        };
        if t >= v {
            return;
        }
        let Some(src) = self.src.clone() else { return };
        let Some(mut full) = src.borrow_mut().full_record(id) else {
            return;
        };
        let r = self.records.get_mut(&id).unwrap();
        // the entries after the snapshot are this working set's
        for (t, f, val) in r.history.iter().filter(|(t, _, _)| *t > v) {
            full.fields.insert(f.clone(), val.clone());
            full.history.push((*t, f.clone(), val.clone()));
        }
        full.formulas = std::mem::take(&mut r.formulas);
        full.pending_defaults = std::mem::take(&mut r.pending_defaults);
        full.frozen_at = r.frozen_at;
        full.warnings = std::mem::take(&mut r.warnings);
        full.cause = r.cause.clone();
        *r = full;
    }

    /// Like `read`, without loading: only what is in the working set.
    pub fn peek(&self, id: Id, key: &str, as_of: Option<u64>) -> Option<V> {
        let r = self.records.get(&id)?;
        match as_of {
            None => r.fields.get(key).cloned(),
            Some(t) => r
                .history
                .iter()
                .rev()
                .find(|(tx, f, _)| *tx <= t && f == key)
                .map(|(_, _, v)| v.clone()),
        }
    }

    pub fn write(&mut self, id: Id, key: &str, v: V) {
        let tx = self.tx;
        self.load(id);
        if let Some(r) = self.records.get_mut(&id) {
            let old = r.fields.insert(key.to_string(), v.clone());
            r.history.push((tx, key.to_string(), v.clone()));
            let ent = r.ent.clone();
            if let Some(&n) = self.keep.get(&*ent) {
                trim_history(&mut r.history, key, tx, n);
            }
            self.dirty.insert(id);
            self.writes += 1;
            self.epoch += 1;
            if !self.untracked {
                self.wlog.push((id, Rc::from(key)));
            }
            if self.src.is_some() || !self.ref_index.is_empty() {
                self.note_written(&ent, id, key, old.as_ref(), Some(&v));
            }
        }
    }

    /// A record taken away (its entity's `expire` came): gone from the working set, and never
    /// loaded from the source again.
    pub fn remove_record(&mut self, id: Id) {
        if let Some(r) = self.records.remove(&id) {
            if let Some(ids) = self.by_entity.get_mut(&*r.ent) {
                ids.remove(&id);
            }
        }
        self.dirty.remove(&id);
        self.absent.insert(id);
    }

    pub fn exists(&mut self, id: Id, as_of: Option<u64>) -> bool {
        self.load(id);
        self.records
            .get(&id)
            .map_or(false, |r| as_of.map_or(true, |t| r.created_tx <= t))
    }

    /// Every id of an entity (exact name): loaded from the source the first time.
    pub fn ids_of(&mut self, ent: &str) -> Vec<Id> {
        if let Some(src) = self.src.clone() {
            if !self.complete.contains(ent) {
                let ids = src.borrow_mut().ids(ent);
                self.by_entity
                    .entry(ent.to_string())
                    .or_default()
                    .extend(ids);
                self.complete.insert(ent.to_string());
            }
            self.reads.scans.insert(ent.to_string());
            let ids: Vec<Id> = self
                .by_entity
                .get(ent)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            self.prefetch_as(ent, &ids);
        }
        self.by_entity
            .get(ent)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The ids of an entity known to the working set, without loading anything.
    pub fn known_ids(&self, ent: &str) -> Vec<Id> {
        self.by_entity
            .get(ent)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Ids of records of `ent` whose `field` equals `value` now, using the source's index.
    /// None without a source or when the source cannot look the field up: scan instead.
    pub fn find(&mut self, ent: &str, field: &str, value: &V) -> Option<Vec<Id>> {
        let src = self.src.clone()?;
        if self.complete.contains(ent) {
            return None;
        }
        let cached = match value {
            V::Ref { id, .. } if !self.found.is_empty() => self
                .found
                .get(&(ent.to_string(), field.to_string(), *id))
                .cloned(),
            _ => None,
        };
        let candidates = match cached {
            Some(c) => c,
            None => src.borrow_mut().find(ent, field, value)?,
        };
        if !self.untracked {
            for k in value_keys(value) {
                self.reads
                    .find_values
                    .insert((ent.to_string(), field.to_string(), k));
            }
        }
        let mut out = Vec::with_capacity(candidates.len());
        self.prefetch_as(ent, &candidates);
        // committed candidates, as they are in the working set
        for id in candidates {
            if self.load(id) && same(self.records[&id].fields.get(field), value) {
                out.push(id);
            }
        }
        // records created here, or whose field changed here, that match now (the others
        // are among the committed candidates)
        if !self.written_by.is_empty() {
            let seen: HashSet<Id> = out.iter().copied().collect();
            let written: Vec<Id> = self
                .written_by
                .get(&(Rc::from(ent), field.to_string()))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            for id in written {
                if !seen.contains(&id)
                    && self
                        .records
                        .get(&id)
                        .map_or(false, |r| same(r.fields.get(field), value))
                {
                    out.push(id);
                }
            }
        }
        out.sort();
        Some(out)
    }

    /// Look up at once the records of `ent` whose `field` refers to each of `targets` (the lines
    /// of a page of records), and load them: the `find`s that follow are answered from here.
    pub fn prefetch_find(&mut self, ent: &str, field: &str, targets: &[Id]) -> Vec<Id> {
        let Some(src) = self.src.clone() else {
            return Vec::new();
        };
        if self.complete.contains(ent) {
            return Vec::new();
        }
        let mut all = Vec::new();
        let mut todo = Vec::new();
        for t in targets {
            match self.found.get(&(ent.to_string(), field.to_string(), *t)) {
                Some(ids) => all.extend(ids.iter().copied()),
                None => todo.push(*t),
            }
        }
        if !todo.is_empty() {
            let Some(map) = src.borrow_mut().find_many(ent, field, &todo) else {
                return Vec::new();
            };
            for t in todo {
                let ids = map.get(&t).cloned().unwrap_or_default();
                all.extend(ids.iter().copied());
                self.found
                    .insert((ent.to_string(), field.to_string(), t), ids);
            }
        }
        all.sort();
        all.dedup();
        self.prefetch_as(ent, &all);
        all
    }

    /// Rows of a ledger (with a source: loaded the first time, by filter when given).
    pub fn ledger_rows(&mut self, ledger: &str, filter: Option<(&str, &V)>) -> Vec<LedgerRow> {
        if let Some(src) = self.src.clone() {
            let all = format!("{ledger}|*");
            let key = match filter {
                Some((c, v)) => format!("{ledger}|{c}|{}", json_key(v)),
                None => all.clone(),
            };
            if !self.rows_loaded.contains(&all) && !self.rows_loaded.contains(&key) {
                for r in src.borrow_mut().rows(ledger, filter) {
                    if self.row_ids.insert(r.id) {
                        self.rows.push(r);
                    }
                }
                self.rows.sort_by_key(|r| r.id);
                self.rows_loaded.insert(key);
            }
            self.reads.ledgers.insert((
                ledger.to_string(),
                filter.map(|(c, v)| (c.to_string(), json_key(v))),
            ));
        }
        self.rows
            .iter()
            .filter(|r| {
                &*r.ledger == ledger && filter.map_or(true, |(c, v)| same(r.cols.get(c), v))
            })
            .cloned()
            .collect()
    }

    pub fn ledger_row(&mut self, id: Id) -> Option<LedgerRow> {
        if let Some(r) = self.rows.iter().find(|r| r.id == id) {
            return Some(r.clone());
        }
        let r = self.src.clone()?.borrow_mut().row(id)?;
        if self.row_ids.insert(r.id) {
            self.rows.push(r.clone());
            self.rows.sort_by_key(|r| r.id);
        }
        Some(r)
    }

    pub fn push_row(&mut self, r: LedgerRow) {
        self.row_ids.insert(r.id);
        self.rows.push(r);
    }

    /// Take back rows posted earlier in the current transaction (replaced before the commit).
    pub fn unpost(&mut self, ids: &[Id]) {
        self.rows.retain(|r| !ids.contains(&r.id));
        self.posted.retain(|r| !ids.contains(r));
        for id in ids {
            self.row_ids.remove(id);
        }
    }

    /// Postings of a record in a ledger.
    pub fn postings_of(&mut self, record: Id, ledger_key: &str) -> Vec<HashMap<String, V>> {
        if let Some(src) = self.src.clone() {
            if self.postings_loaded.insert(record) {
                for (k, rows) in src.borrow_mut().postings(record) {
                    self.postings.entry((record, k)).or_insert(rows);
                }
            }
        }
        self.postings
            .get(&(record, ledger_key.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// The next value of a counter (incremented).
    pub fn bump_counter(&mut self, key: &str) -> u64 {
        if let Some(src) = self.src.clone() {
            if self.counters_loaded.insert(key.to_string()) {
                let n = src.borrow_mut().counter(key);
                self.counters.entry(key.to_string()).or_insert(n);
                self.reads.counters.insert(key.to_string(), n);
            }
        }
        let n = self.counters.entry(key.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    /// When and by whom transaction `tx` was made (from the source when it is not known here).
    pub fn commit_info(&mut self, tx: u64) -> Option<(NaiveDateTime, Option<Id>)> {
        if let Some(c) = self.commits.get(&tx) {
            return Some(*c);
        }
        let c = self.src.clone()?.borrow_mut().commit(tx)?;
        self.commits.insert(tx, c);
        Some(c)
    }

    /// The first error met by the source while loading.
    pub fn source_error(&self) -> Option<String> {
        self.src.as_ref().and_then(|s| s.borrow_mut().error())
    }

    /// After a commit that got the database transaction number `to` for the local `from`.
    pub fn renumber(&mut self, from: u64, to: u64) {
        if from == to {
            return;
        }
        let fix = |t: &mut u64| {
            if *t == from {
                *t = to;
            }
        };
        for id in self.dirty.clone() {
            if let Some(r) = self.records.get_mut(&id) {
                fix(&mut r.created_tx);
                for h in r.history.iter_mut() {
                    fix(&mut h.0);
                }
                if let Some(f) = r.frozen_at.as_mut() {
                    fix(f);
                }
            }
        }
        for r in self.rows.iter_mut() {
            fix(&mut r.tx);
        }
        for t in self.applied.values_mut() {
            fix(t);
        }
        if let Some(c) = self.commits.remove(&from) {
            self.commits.insert(to, c);
        }
        for r in self.renames.iter_mut() {
            fix(&mut r.3);
        }
        self.tx = to;
    }
}

/// The versions of `key` written before `tx` beyond the last `n` dropped (those of `tx` stay:
/// the transaction still compares with the value before it).
fn trim_history(h: &mut Vec<(u64, String, V)>, key: &str, tx: u64, n: usize) {
    let before = h.iter().filter(|(t, f, _)| *t < tx && f == key).count();
    if before <= n {
        return;
    }
    let mut drop = before - n;
    h.retain(|(t, f, _)| {
        if drop > 0 && *t < tx && f == key {
            drop -= 1;
            false
        } else {
            true
        }
    });
}
