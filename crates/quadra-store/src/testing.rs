//! Running the tests on a real database: each test gets a fresh database; at the end the
//! state is reloaded from it and compared with the interpreter, then `verify` runs.

use crate::conn::Conn;
use crate::db::{materialized_formulas, Database, Sink};
use crate::schema::Schema;
use crate::source::{Cache, DbSource};
use quadra_ir::Model;
use quadra_runtime::store::Store;
use quadra_runtime::{Interp, TestBackend};
use quadra_types::Report;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Distinguishes backends running in the same process (parallel tests).
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct DbBackend {
    /// `sqlite` (temporary files) or a `postgres://` url (a schema per test)
    pub target: String,
    /// load on demand (a fresh working set before each statement, a shared cache), or keep
    /// everything in memory and only write through
    pub lazy: bool,
    /// last commit hash of each test's database (test index, hash): equal hashes on two
    /// backends mean byte-identical history and ledger rows
    pub hashes: Vec<(usize, String)>,
    /// how many records were served by the cache and loaded from the database
    pub cache_hits: u64,
    pub cache_misses: u64,
    tag: String,
    index: usize,
    schema: Option<Rc<Schema>>,
    cache: Option<Rc<RefCell<Cache>>>,
    /// `quadra test --cold`: before each statement, everything closed goes cold (whatever its
    /// term), so the tests read and write through the volumes: the results must be the same
    pub cold: bool,
    model: Option<(quadra_ir::Model, Report)>,
}

impl DbBackend {
    pub fn new(target: &str) -> DbBackend {
        DbBackend {
            target: target.to_string(),
            lazy: true,
            hashes: Vec::new(),
            cache_hits: 0,
            cache_misses: 0,
            tag: format!(
                "{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            index: 0,
            schema: None,
            cache: None,
            cold: false,
            model: None,
        }
    }

    /// Everything in memory, written through to the database (the reference behaviour).
    pub fn in_memory(target: &str) -> DbBackend {
        DbBackend {
            lazy: false,
            ..DbBackend::new(target)
        }
    }

    fn url(&self, index: usize) -> String {
        if self.target == "sqlite" {
            let p = std::env::temp_dir().join(format!("quadra-test-{}-{index}.db", self.tag));
            format!("sqlite:{}", p.display())
        } else {
            format!("{}#schema=qt_{}_{index}", self.target, self.tag)
        }
    }

    fn cleanup(&self, index: usize) {
        if self.target == "sqlite" {
            let p = std::env::temp_dir().join(format!("quadra-test-{}-{index}.db", self.tag));
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
            }
        } else if let Ok(mut c) = Conn::open(&self.target) {
            let _ = c.batch(&format!(
                "DROP SCHEMA IF EXISTS \"qt_{}_{index}\" CASCADE",
                self.tag
            ));
        }
    }

    fn working_set(&self) -> Result<Store, String> {
        let src = DbSource::open(
            &self.url(self.index),
            self.schema.clone().unwrap(),
            self.cache.clone(),
        )?;
        let mut store = Store::over(Rc::new(RefCell::new(src)));
        // one id at a time: the same ids as in memory, so the same hash chain
        store.id_block_size = 1;
        Ok(store)
    }
}

impl TestBackend for DbBackend {
    fn attach<'m>(
        &mut self,
        model: &'m Model,
        report: &'m Report,
        index: usize,
        interp: &mut Interp<'m>,
    ) -> Result<(), String> {
        self.cleanup(index);
        self.index = index;
        let mut db = Database::open(&self.url(index), model, report)?;
        db.init()?;
        db.seed_salts()?;
        interp.materialize = materialized_formulas(&db.schema);
        self.schema = Some(db.schema.clone());
        if self.cold {
            self.model = Some((model.clone(), report.clone()));
        }
        interp.sink = Some(Box::new(Sink { db, model }));
        if self.lazy {
            self.cache = Some(Rc::new(RefCell::new(Cache::new(
                8 << 20,
                std::time::Duration::from_secs(300),
            ))));
            interp.store = self.working_set()?;
        }
        Ok(())
    }

    fn refresh<'m>(&mut self, interp: &mut Interp<'m>) -> Result<(), String> {
        if self.cold && self.lazy {
            if let Some((m, r)) = &self.model {
                let url = self.url(self.index);
                let time = interp.clock;
                quadra_runtime::cold_force.with(|f| f.set(true));
                let moved =
                    crate::ops::cold_run(&url, m, r, time, None, &mut |_, _| Ok(()), &mut |_| {
                        Ok(())
                    });
                quadra_runtime::cold_force.with(|f| f.set(false));
                moved?;
            }
        }
        if self.lazy {
            interp.store = self.working_set()?;
        }
        Ok(())
    }

    fn check<'m>(
        &mut self,
        model: &'m Model,
        report: &'m Report,
        index: usize,
        interp: &mut Interp<'m>,
    ) -> Result<(), String> {
        interp.sink = None; // close the connection
        let mut db = Database::open(&self.url(index), model, report)?;
        let loaded = db.load()?;
        self.hashes
            .push((index, db.status()?.last_hash.unwrap_or_default()));
        let mut problems = if self.lazy {
            compare_working_set(&interp.store, &loaded)
        } else {
            compare(&interp.store, &loaded)
        };
        if problems.is_empty() {
            // projections against a full recomputation from the history
            let mut full = Interp::new(model);
            full.store = loaded;
            full.load_units();
            full.materialize = materialized_formulas(&db.schema);
            full.materialize_formulas();
            problems.extend(db.verify(model, &full.store)?);
        }
        if let Some(c) = &self.cache {
            self.cache_hits += c.borrow().hits;
            self.cache_misses += c.borrow().misses;
        }
        interp.store = Store::default();
        drop(db);
        match std::env::var("QUADRA_KEEP_DB") {
            Ok(_) => eprintln!("kept {}", self.url(index)),
            Err(_) => self.cleanup(index),
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the database does not match the engine:\n      {}",
                problems
                    .into_iter()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("\n      ")
            ))
        }
    }
}

/// The records of a working set loaded on demand, against the database.
pub fn compare_working_set(ws: &Store, db: &Store) -> Vec<String> {
    let mut out = Vec::new();
    for r in ws.sorted() {
        let id = &r.id;
        let Some(s) = db.records.get(id) else {
            out.push(format!("record {}#{id} missing", r.ent));
            continue;
        };
        // a field without a value and one whose value is none are the same (a record read
        // from its row has no key for an empty column)
        let set = |f: &rustc_hash::FxHashMap<String, quadra_runtime::V>| {
            f.iter()
                .filter(|(_, v)| !v.is_none())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        if set(&r.fields) != set(&s.fields) {
            out.push(format!("{}#{id}: fields differ", r.ent));
        }
        // a record read from its projection row has only its values since its last write
        if r.partial.is_none() && r.history.len() != s.history.len() {
            out.push(format!(
                "{}#{id}: {} history entries here, {} in the database",
                r.ent,
                r.history.len(),
                s.history.len()
            ));
        }
        if r.version() != s.version() {
            out.push(format!(
                "{}#{id}: version {} here, {} in the database",
                r.ent,
                r.version(),
                s.version()
            ));
        }
    }
    out
}

/// Differences between the engine's store and the store reloaded from the database.
pub fn compare(a: &Store, b: &Store) -> Vec<String> {
    let mut out = Vec::new();
    if a.records.len() != b.records.len() {
        out.push(format!(
            "{} records in memory, {} in the database",
            a.records.len(),
            b.records.len()
        ));
    }
    for r in a.sorted() {
        let id = &r.id;
        let Some(s) = b.records.get(id) else {
            out.push(format!("record {}#{id} missing", r.ent));
            continue;
        };
        if r.ent != s.ent
            || r.created_tx != s.created_tx
            || r.frozen_at != s.frozen_at
            || r.warnings != s.warnings
        {
            out.push(format!("record {}#{id}: metadata differs", r.ent));
        }
        let mut keys: Vec<&String> = r.fields.keys().chain(s.fields.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            if r.fields.get(k) != s.fields.get(k) {
                out.push(format!(
                    "{}#{id}.{k}: {:?} in memory, {:?} in the database",
                    r.ent,
                    r.fields.get(k).map(|v| v.to_string()),
                    s.fields.get(k).map(|v| v.to_string())
                ));
            }
        }
        if r.history.len() != s.history.len() {
            out.push(format!(
                "{}#{id}: {} history entries in memory, {} in the database",
                r.ent,
                r.history.len(),
                s.history.len()
            ));
        }
        if r.cause.chain() != s.cause.chain() {
            out.push(format!("{}#{id}: cause differs", r.ent));
        }
    }
    if a.rows.len() != b.rows.len() {
        out.push(format!(
            "{} ledger rows in memory, {} in the database",
            a.rows.len(),
            b.rows.len()
        ));
    } else {
        for (x, y) in a.rows.iter().zip(b.rows.iter()) {
            if x.id != y.id || x.cols != y.cols || x.source != y.source || x.tx != y.tx {
                out.push(format!("ledger row {} differs", x.id));
            }
        }
    }
    if a.applied != b.applied {
        out.push(format!(
            "applied scripts differ: {:?} / {:?}",
            a.applied, b.applied
        ));
    }
    if a.renames != b.renames {
        out.push("renames differ".into());
    }
    if a.counters != b.counters {
        out.push("counters differ".into());
    }
    if a.postings != b.postings {
        out.push("postings differ".into());
    }
    // ids are not reused after a rollback, so memory may be ahead of the last commit
    if a.next_id < b.next_id || a.tx != b.tx {
        out.push(format!(
            "state differs: next id {} / {}, tx {} / {}",
            a.next_id, b.next_id, a.tx, b.tx
        ));
    }
    out
}
