//! `quadra db verify`, in bounded memory: the hash chain and the projections are read a page
//! at a time, whatever the size of the database.
//!
//! - **The chain.** The commits in order, each hashed from its history entries, ledger rows,
//!   applied migrations and renames, read as streams ordered by `seq`, `id` (the order commits
//!   write them, so each commit's entries come together) and merged by transaction. On a
//!   connection and a thread of its own, while the tables are checked.
//! - **The projections.** Each entity table is cut into ranges of ids (`UNIT_ROWS` rows),
//!   checked by several workers (half the cores, at most 6; `QUADRA_VERIFY_WORKERS`), each with
//!   its connection and engine, a page of rows at a time: the records of the page read from
//!   their history (`DbSource::history_ids`) with their formulas computed by the engine, the
//!   records they read (lines, references) from their rows (`DbSource::rows_trusted`: those
//!   rows are checked on their own pages). When some rows do not say what their history says,
//!   the next pass reads those records from their history wherever they are read. A formula
//!   reads the values of the formulas of other records from their rows (only the page's own
//!   are computed): when they are right, that is what computing everything from the history
//!   gives. When some are not, the pass is done again with the values computed for them
//!   (`Store::formula_overrides`) until the wrong values found are the same twice: then every
//!   value read was the one computed from the history, and the findings are those of
//!   computing every formula from scratch. The findings come in the order of the tables and
//!   ids, whichever worker found them.

use crate::conn::{Conn, Pooled, Sql};
use crate::db::{encode, history_entry, materialized_formulas, same, CommitHasher, Database};
use crate::schema::{Source as TableSource, Table};
use crate::source::{head, load_renames, DbSource};
use chrono::NaiveDateTime;
use quadra_ir::Model;
use quadra_runtime::{Id, Interp, SharedSource, Store, V};
use quadra_types::Report;
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;

/// Rows of a table read and checked together.
pub static PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1_000);

/// Records a page loads at most, about (the records of the page and what their formulas
/// read): the page has fewer rows when they read many.
const LOADED: usize = 10_000;

/// The passes over the projections at most (a chain of wrong formula values one more each).
const MAX_PASSES: usize = 12;

fn num(s: &Option<String>) -> i64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

fn page() -> usize {
    PAGE.load(std::sync::atomic::Ordering::Relaxed).max(1)
}

/// Hash chain and tables against the history. Returns the problems and the record count.
pub fn verify(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
) -> Result<(Vec<String>, usize), String> {
    verify_with(url, model, report, time, None)
}

/// `verify`, with the rows of the archives read back (`quadra db verify --full`): every commit
/// computed, those before the checkpoints too. Without them, the commits whose rows went to an
/// archive are checked by their links and signatures only.
pub fn verify_with(
    url: &str,
    model: &Model,
    report: &Report,
    time: NaiveDateTime,
    archived: Option<Vec<crate::archive::Row>>,
) -> Result<(Vec<String>, usize), String> {
    let mut db = Database::open(url, model, report)?;
    // the chain on a connection of its own, while the tables are checked
    let chain_url = url.to_string();
    let chain_check = std::thread::Builder::new()
        .name("quadra-verify-chain".into())
        .spawn(move || chain(&mut Conn::open(&chain_url)?, archived))
        .map_err(|e| e.to_string())?;
    // every write of the history, those of a transaction without commit row too (as loading
    // the whole database reads them)
    let latest = db.conn.query(
        "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_history\"",
        &[],
    )?;
    let created = db.conn.query(
        "SELECT CAST(MAX(\"created_tx\") AS TEXT) FROM \"_quadra_records\"",
        &[],
    )?;
    let base = head(&mut db.conn)?
        .max(num(&latest.first().and_then(|r| r[0].clone())) as u64)
        .max(num(&created.first().and_then(|r| r[0].clone())) as u64);
    let records = num(&db
        .conn
        .query(
            "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_records\"",
            &[],
        )?
        .first()
        .and_then(|r| r[0].clone())) as usize;
    let (mut problems, tables) = projections(url, &mut db, model, time, base, Some(chain_check))?;
    problems.extend(tables);
    Ok((problems, records))
}

/// Entries of the commits read in order: (transaction, the entry as the hash has it).
struct Stream {
    sql: &'static str,
    after: i64,
    buf: VecDeque<(i64, String)>,
    done: bool,
    /// a row read: (its key for the next page, its transaction, the entry)
    row: fn(&[Option<String>]) -> (i64, i64, String),
}

impl Stream {
    fn front(&mut self, conn: &mut Conn) -> Result<Option<i64>, String> {
        if self.buf.is_empty() && !self.done {
            let n = page() * 10;
            let rows = conn.query(self.sql, &[Sql::Int(self.after), Sql::Int(n as i64)])?;
            self.done = rows.len() < n;
            for r in rows {
                let (key, tx, entry) = (self.row)(&r);
                self.after = key;
                self.buf.push_back((tx, entry));
            }
        }
        Ok(self.buf.front().map(|(tx, _)| *tx))
    }

    /// The entries of commit `tx` to its hash; those of earlier transactions have no commit.
    fn feed(
        &mut self,
        conn: &mut Conn,
        tx: i64,
        hasher: &mut CommitHasher,
        orphans: &mut BTreeSet<i64>,
    ) -> Result<(), String> {
        while let Some(t) = self.front(conn)? {
            if t > tx {
                break;
            }
            let (t, entry) = self.buf.pop_front().unwrap();
            if t < tx {
                orphans.insert(t);
            } else {
                hasher.entry(&entry);
            }
        }
        Ok(())
    }
}

/// The hash chain, commit by commit (`commit_hash`), streaming what each commit wrote.
fn chain(
    conn: &mut Conn,
    archived: Option<Vec<crate::archive::Row>>,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    // the checkpoints: the commits up to the last one's `before_tx` are computed only with the
    // rows of the archives
    let checkpoints = crate::archive::checkpoints(conn)?;
    let archived_to = checkpoints
        .iter()
        .map(|c| c.before_tx as i64)
        .max()
        .unwrap_or(0);
    let full = archived.is_some();
    if let Some(rows) = archived {
        conn.batch("CREATE TEMP TABLE \"_qx_archived\" (\"seq\" BIGINT, \"tx\" BIGINT, \"record_id\" BIGINT, \"field\" TEXT, \"value\" TEXT, \"salt\" TEXT, \"digest\" TEXT)")?;
        for chunk in rows.chunks(500) {
            let mut params = Vec::new();
            let mut values = Vec::new();
            for (seq, tx, rec, f, v, salt, d) in chunk {
                values.push("(?bigint, ?bigint, ?bigint, ?text, ?text, ?text, ?text)");
                params.extend([
                    Sql::Int(*seq),
                    Sql::Int(*tx),
                    Sql::Int(*rec),
                    Sql::Text(f.clone()),
                    v.clone().map(Sql::Text).unwrap_or(Sql::Null),
                    salt.clone().map(Sql::Text).unwrap_or(Sql::Null),
                    d.clone().map(Sql::Text).unwrap_or(Sql::Null),
                ]);
            }
            conn.exec(
                &format!("INSERT INTO \"_qx_archived\" VALUES {}", values.join(", ")),
                &params,
            )?;
        }
    }
    let mut hist = Stream {
        sql: if full {
            "SELECT CAST(\"seq\" AS TEXT), CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\", \"salt\", \"digest\" FROM (SELECT \"seq\", \"tx\", \"record_id\", \"field\", \"value\", \"salt\", \"digest\" FROM \"_quadra_history\" UNION ALL SELECT \"seq\", \"tx\", \"record_id\", \"field\", \"value\", \"salt\", \"digest\" FROM \"_qx_archived\") h WHERE \"seq\" > ?bigint ORDER BY \"seq\" LIMIT ?bigint"
        } else {
            "SELECT CAST(\"seq\" AS TEXT), CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\", \"salt\", \"digest\" FROM \"_quadra_history\" WHERE \"seq\" > ?bigint ORDER BY \"_quadra_history\".\"seq\" LIMIT ?bigint"
        },
        after: i64::MIN,
        buf: VecDeque::new(),
        done: false,
        row: |r| {
            (
                num(&r[0]),
                num(&r[1]),
                history_entry(
                    json!(num(&r[2])),
                    r[3].as_deref().unwrap_or(""),
                    r[4].as_deref(),
                    r[5].as_deref(),
                    r[6].as_deref(),
                )
                .to_string(),
            )
        },
    };
    let mut ledger = Stream {
        sql: "SELECT CAST(\"id\" AS TEXT), CAST(\"tx\" AS TEXT), \"ledger\", \"cols\" FROM \"_quadra_ledger_rows\" WHERE \"id\" > ?bigint ORDER BY \"_quadra_ledger_rows\".\"id\" LIMIT ?bigint",
        after: i64::MIN,
        buf: VecDeque::new(),
        done: false,
        row: |r| (num(&r[0]), num(&r[1]), json!(["r", num(&r[0]), r[2].clone().unwrap_or_default(), r[3].clone().unwrap_or_default()]).to_string()),
    };
    // applied migrations and renames: a few rows per migration
    let mut applied: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    let mut keys: Vec<(i64, String)> = conn
        .query(
            "SELECT CAST(\"tx\" AS TEXT), \"key\" FROM \"_quadra_applied\"",
            &[],
        )?
        .iter()
        .map(|r| (num(&r[0]), r[1].clone().unwrap_or_default()))
        .collect();
    keys.sort();
    for (tx, k) in keys {
        applied
            .entry(tx)
            .or_default()
            .push(json!(["a", k]).to_string());
    }
    // erasures: a few rows per erasure, after the ledger rows of their commit
    let mut erasures: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for r in conn.query("SELECT CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"entity\", \"fields\", CAST(\"whole\" AS TEXT), \"reason\" FROM \"_quadra_erasures\" ORDER BY \"_quadra_erasures\".\"tx\", \"_quadra_erasures\".\"n\"", &[])? {
        let whole = matches!(r[4].as_deref(), Some("1" | "true" | "t"));
        erasures.entry(num(&r[0])).or_default().push(
            crate::db::erasure_entry(num(&r[1]) as u64, r[2].as_deref().unwrap_or(""), r[3].as_deref().unwrap_or(""), whole, r[5].as_deref().unwrap_or("")).to_string(),
        );
    }
    let mut renames: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for r in conn.query("SELECT CAST(\"tx\" AS TEXT), \"entity\", \"old_name\", \"new_name\" FROM \"_quadra_renames\" ORDER BY \"_quadra_renames\".\"seq\"", &[])? {
        renames.entry(num(&r[0])).or_default().push(json!(["n", r[1], r[2], r[3]]).to_string());
    }
    let mut keys = crate::signing::Keys::read(conn)?;
    let mut orphans: BTreeSet<i64> = BTreeSet::new();
    let mut prev = "0".repeat(64);
    let mut after = i64::MIN;
    loop {
        let rows = conn.query(
            "SELECT CAST(\"tx\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT), \"cause\", \"prev_hash\", \"hash\", \"signature\" FROM \"_quadra_commits\" WHERE \"tx\" > ?bigint ORDER BY \"_quadra_commits\".\"tx\" LIMIT 1000",
            &[Sql::Int(after)],
        )?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            let tx = num(&r[0]);
            after = tx;
            if r[4].as_deref() != Some(prev.as_str()) {
                problems.push(format!(
                    "commit {tx}: previous hash does not match the chain"
                ));
            }
            let user = r[2].as_deref().and_then(|u| u.parse().ok());
            let mut h = CommitHasher::new(
                &prev,
                tx as u64,
                r[1].as_deref().unwrap_or(""),
                user,
                r[3].as_deref().unwrap_or(""),
            );
            hist.feed(conn, tx, &mut h, &mut orphans)?;
            ledger.feed(conn, tx, &mut h, &mut orphans)?;
            for e in erasures.remove(&tx).unwrap_or_default() {
                h.entry(&e);
            }
            // (those of earlier transactions have no commit)
            let earlier: Vec<i64> = applied
                .range(..tx)
                .map(|(t, _)| *t)
                .chain(renames.range(..tx).map(|(t, _)| *t))
                .collect();
            for t in earlier {
                applied.remove(&t);
                renames.remove(&t);
                orphans.insert(t);
            }
            for e in applied
                .remove(&tx)
                .unwrap_or_default()
                .iter()
                .chain(renames.remove(&tx).unwrap_or_default().iter())
            {
                h.entry(e);
            }
            if let Some(e) = keys.entry(tx) {
                h.entry(&e);
            }
            if let Some(c) = checkpoints.iter().find(|c| c.tx as i64 == tx) {
                h.entry(&crate::archive::entry(c).to_string());
            }
            let computed = h.finish();
            // (a commit some of whose rows are in an archive: computed with `--full` only)
            if (full || tx > archived_to) && r[5].as_deref() != Some(computed.as_str()) {
                problems.push(format!("commit {tx}: content does not match its hash (history or ledger rows were altered)"));
            }
            if let Some(p) = keys.check(tx, r[5].as_deref().unwrap_or(""), r[6].as_deref()) {
                problems.push(p);
            }
            prev = r[5].clone().unwrap_or_default();
        }
    }
    for s in [&mut hist, &mut ledger] {
        while let Some(t) = s.front(conn)? {
            orphans.insert(t);
            s.buf.pop_front();
        }
    }
    orphans.extend(applied.keys().chain(renames.keys()));
    for tx in orphans {
        problems.push(format!("writes of transaction {tx} have no commit row"));
    }
    if let Some(p) = keys.last(crate::signing::expected().as_deref()) {
        problems.push(p);
    }
    Ok(problems)
}

/// What `quadra db verify` says besides the problems: the values erased (their digests kept),
/// the commits signed and the key signing them.
pub fn facts(url: &str) -> Result<(u64, u64, Option<String>), String> {
    let mut conn = Conn::open(url)?;
    let one = |conn: &mut Conn, sql: &str| -> Result<u64, String> {
        Ok(num(&conn.query(sql, &[])?.first().and_then(|r| r[0].clone())) as u64)
    };
    let erased = one(
        &mut conn,
        "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_history\" WHERE \"digest\" IS NOT NULL",
    )?;
    let signed = one(
        &mut conn,
        "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_commits\" WHERE \"signature\" IS NOT NULL",
    )?;
    Ok((erased, signed, crate::signing::active(&mut conn)?))
}

/// Rows of a table checked by one unit of work (a range of ids): the units are spread over
/// the workers.
const UNIT_ROWS: usize = 10_000;

/// A range of rows of an entity table: ids in `(after, upto]`.
struct Unit {
    table: usize,
    after: i64,
    upto: Option<i64>,
}

/// What a unit found: the stored values and rows that do not say what the history says, the
/// formulas whose column does not hold what they compute (with that value, as JSON: it goes
/// from a worker to the next pass), and the rows whose lookups must take the record.
#[derive(Default)]
struct UnitOut {
    stored: Vec<String>,
    problems: Vec<String>,
    wrong: Vec<((Id, String), serde_json::Value)>,
    patch: Vec<((String, String), Id)>,
}

/// What every worker of a pass shares.
struct Pass<'a> {
    url: &'a str,
    model: &'a Model,
    schema: &'a crate::schema::Schema,
    renames: &'a crate::source::Renames,
    time: NaiveDateTime,
    base: u64,
    first: bool,
    tables: &'a [Table],
    units: &'a [Unit],
    overrides: &'a HashMap<Id, Vec<(String, serde_json::Value)>>,
    patch: &'a HashMap<(String, String), Vec<Id>>,
    wrong_rows: &'a std::collections::HashSet<Id>,
}

/// Workers checking the units of a pass, each with its connection, engine and cache.
fn workers() -> usize {
    std::env::var("QUADRA_VERIFY_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(
            (std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2)
                / 2)
            .clamp(1, 6),
        )
}

/// Every entity table against the history, a range of rows at a time, the ranges checked by
/// several workers (see the module). The first pass also checks the stored values and finds
/// the rows whose references (or sets) do not say what the history says, and the records
/// without a row: the lookups of the next passes take them as candidates too
/// (`DbSource::patch`), and they are read from their history, so that a collection is the one
/// of the history whatever the tables say. The findings come in the order of the tables and
/// of the ids, whatever the worker that found them.
fn projections(
    url: &str,
    db: &mut Database,
    model: &Model,
    time: NaiveDateTime,
    base: u64,
    chain: Option<std::thread::JoinHandle<Result<Vec<String>, String>>>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let schema = db.schema.clone();
    let renames = load_renames(&mut db.conn)?;
    let tables: Vec<Table> = schema
        .tables
        .iter()
        .filter(|t| matches!(t.source, TableSource::Entity(_)))
        .cloned()
        .collect();
    // the ranges of rows: about `UNIT_ROWS` each (smaller when `PAGE` is set lower, as the
    // tests do to cross page boundaries)
    let per_unit = UNIT_ROWS.min(page() * 10).max(1);
    let mut units: Vec<Unit> = Vec::new();
    let mut table_msgs: Vec<Vec<String>> = vec![Vec::new(); tables.len()];
    let mut patch: HashMap<(String, String), BTreeSet<Id>> = HashMap::new();
    for (ti, t) in tables.iter().enumerate() {
        let TableSource::Entity(ent) = &t.source else {
            continue;
        };
        let n_rows = num(&db
            .conn
            .query(
                &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"{}\"", t.name),
                &[],
            )?
            .first()
            .and_then(|r| r[0].clone()));
        let n_recs = num(&db
            .conn
            .query(
                "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text",
                &[Sql::Text(ent.clone())],
            )?
            .first()
            .and_then(|r| r[0].clone()));
        if n_rows != n_recs {
            table_msgs[ti].push(format!(
                "table {}: {} rows, the history has {}",
                t.name, n_rows, n_recs
            ));
            // the records without a row: candidates of every lookup of their entity
            let missing = db.conn.query(
                &format!("SELECT CAST(r.\"id\" AS TEXT) FROM \"_quadra_records\" r WHERE r.\"entity\" = ?text AND NOT EXISTS (SELECT 1 FROM \"{}\" t WHERE t.\"id\" = r.\"id\")", t.name),
                &[Sql::Text(ent.clone())],
            )?;
            let sets = schema.sets.get(ent).cloned().unwrap_or_default();
            for r in missing {
                let id = num(&r[0]) as Id;
                for fm in t.fields.iter().filter(|fm| !fm.formula) {
                    patch
                        .entry((ent.clone(), fm.field.clone()))
                        .or_default()
                        .insert(id);
                }
                for sm in &sets {
                    patch
                        .entry((ent.clone(), sm.field.clone()))
                        .or_default()
                        .insert(id);
                }
            }
        }
        // the last id of every `per_unit` rows: the bounds of the units
        let mut bounds: Vec<i64> = Vec::new();
        if n_rows as usize > per_unit {
            for r in db.conn.query(&format!("SELECT CAST(q.\"id\" AS TEXT) FROM (SELECT \"id\", ROW_NUMBER() OVER (ORDER BY \"id\") AS rn FROM \"{}\") q WHERE q.rn % {per_unit} = 0 ORDER BY q.\"id\"", t.name), &[])? {
                bounds.push(num(&r[0]));
            }
        }
        let mut after = i64::MIN;
        for b in bounds {
            units.push(Unit {
                table: ti,
                after,
                upto: Some(b),
            });
            after = b;
        }
        units.push(Unit {
            table: ti,
            after,
            upto: None,
        });
    }
    let mut stored_problems: Vec<String> = Vec::new();
    let mut overrides: HashMap<(Id, String), serde_json::Value> = HashMap::new();
    let mut chain = chain;
    let mut chain_problems: Vec<String> = Vec::new();
    for pass in 0..MAX_PASSES {
        let first = pass == 0;
        let mut by_id: HashMap<Id, Vec<(String, serde_json::Value)>> = HashMap::new();
        for ((id, f), v) in &overrides {
            by_id.entry(*id).or_default().push((f.clone(), v.clone()));
        }
        let patch_list: HashMap<(String, String), Vec<Id>> = patch
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().copied().collect()))
            .collect();
        // the records whose rows do not say what their history says (found by the first pass):
        // read from their history wherever they are read
        let wrong_rows: std::collections::HashSet<Id> = patch.values().flatten().copied().collect();
        let patched = patch.len();
        let ctx = Pass {
            url,
            model,
            schema: &schema,
            renames: &renames,
            time,
            base,
            first,
            tables: &tables,
            units: &units,
            overrides: &by_id,
            patch: &patch_list,
            wrong_rows: &wrong_rows,
        };
        let outs = run_units(&ctx)?;
        if let Some(h) = chain.take() {
            chain_problems = h
                .join()
                .map_err(|_| "verify: the hash chain's check failed".to_string())??;
        }
        let mut problems = Vec::new();
        let mut wrong: HashMap<(Id, String), serde_json::Value> = HashMap::new();
        let mut table_done = vec![false; tables.len()];
        for (u, out) in units.iter().zip(outs) {
            if first && !table_done[u.table] {
                table_done[u.table] = true;
                stored_problems.extend(table_msgs[u.table].iter().cloned());
            }
            stored_problems.extend(out.stored);
            problems.extend(out.problems);
            wrong.extend(out.wrong);
            for (k, id) in out.patch {
                patch.entry(k).or_default().insert(id);
            }
        }
        // settled: the lookups were the history's, and the values read were those computed
        if wrong == overrides && patch.len() == patched && !(first && !patch.is_empty()) {
            stored_problems.extend(problems);
            return Ok((chain_problems, stored_problems));
        }
        overrides = wrong;
    }
    Err("verify: the formulas found wrong do not settle".into())
}

/// The units of a pass, checked by the workers; their findings in the order of the units.
fn run_units(ctx: &Pass) -> Result<Vec<UnitOut>, String> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<Option<UnitOut>>> =
        std::sync::Mutex::new((0..ctx.units.len()).map(|_| None).collect());
    let n = workers().min(ctx.units.len()).max(1);
    let errors: Vec<String> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                sc.spawn(|| -> Result<(), String> {
                    let mut w = Worker::new(ctx)?;
                    loop {
                        let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if k >= ctx.units.len() {
                            return Ok(());
                        }
                        let out = w.unit(ctx, &ctx.units[k]);
                        match out {
                            Ok(o) => results.lock().unwrap()[k] = Some(o),
                            Err(e) => {
                                // the others stop at their next unit
                                next.store(ctx.units.len(), std::sync::atomic::Ordering::Relaxed);
                                return Err(e);
                            }
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("verify: a worker failed".into()))
                    .err()
            })
            .collect()
    });
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|o| o.ok_or_else(|| "verify: a range of rows was not checked".to_string()))
        .collect()
}

/// A worker: its connection (a source reading the page's records from their history and the
/// others from their rows), and its engine.
struct Worker<'m> {
    src: Rc<RefCell<DbSource>>,
    interp: Interp<'m>,
    by_id: Rc<HashMap<Id, Vec<(String, V)>>>,
    /// rows per page: smaller when what their formulas read is large (a product's lines)
    size: HashMap<usize, usize>,
}

impl<'m> Worker<'m> {
    fn new(ctx: &Pass<'m>) -> Result<Worker<'m>, String> {
        let schema = Rc::new(ctx.schema.clone());
        let src = Rc::new(RefCell::new(DbSource::with_renames(
            Pooled::own(Conn::open(ctx.url)?),
            ctx.base,
            schema.clone(),
            None,
            ctx.renames.clone(),
        )?));
        {
            let mut s = src.borrow_mut();
            // the page's records from their history, the other records from their rows (checked
            // on their own pages: those found wrong are read from their history by the next
            // pass), formulas of other records from their rows; queries over a whole entity
            // read its records, not the tables being checked; the ledger tables are not read
            s.rows = true;
            s.fields_from_rows = false;
            s.rows_trusted = true;
            s.sql = false;
            s.patch = ctx.patch.clone();
        }
        let shared: SharedSource = src.clone();
        let mut interp = Interp::new(ctx.model);
        interp.set_clock(ctx.time);
        interp.store = Store::over(shared);
        interp.store.untracked = true;
        interp.load_units();
        interp.materialize = materialized_formulas(&schema);
        let by_id: HashMap<Id, Vec<(String, V)>> = ctx
            .overrides
            .iter()
            .map(|(id, l)| {
                (
                    *id,
                    l.iter()
                        .map(|(f, v)| (f.clone(), quadra_runtime::codec::from_json(v)))
                        .collect(),
                )
            })
            .collect();
        Ok(Worker {
            src,
            interp,
            by_id: Rc::new(by_id),
            size: HashMap::new(),
        })
    }

    /// The rows of a unit, a page at a time.
    fn unit(&mut self, ctx: &Pass<'m>, u: &Unit) -> Result<UnitOut, String> {
        let t = &ctx.tables[u.table];
        let TableSource::Entity(ent) = &t.source else {
            return Ok(UnitOut::default());
        };
        let model = ctx.model;
        let first = ctx.first;
        let mut out = UnitOut::default();
        let lines = lines_read(model, ent);
        // what the page's formulas read is loaded with it (nothing for a table without any)
        let computes = t.fields.iter().any(|f| f.formula);
        let sets = ctx.schema.sets.get(ent).cloned().unwrap_or_default();
        let cols: Vec<String> = t
            .fields
            .iter()
            .flat_map(|f| {
                f.columns
                    .iter()
                    .map(|c| format!("CAST(\"{}\" AS TEXT)", c.name))
            })
            .collect();
        let names: Vec<String> = t
            .fields
            .iter()
            .flat_map(|f| f.columns.iter().map(|c| c.name.clone()))
            .collect();
        let upto = match u.upto {
            Some(b) => format!(" AND \"id\" <= {b}"),
            None => String::new(),
        };
        let sql = format!("SELECT CAST(\"id\" AS TEXT){} FROM \"{}\" WHERE \"id\" > ?bigint{upto} ORDER BY \"{}\".\"id\" LIMIT ?bigint", cols.iter().map(|c| format!(", {c}")).collect::<String>(), t.name, t.name,);
        let mut after = u.after;
        let mut size = *self.size.get(&u.table).unwrap_or(&page().min(64));
        loop {
            let rows = self
                .src
                .borrow_mut()
                .conn()
                .query(&sql, &[Sql::Int(after), Sql::Int(size as i64)])?;
            if rows.is_empty() {
                break;
            }
            let ids: Vec<Id> = rows.iter().map(|r| num(&r[0]) as Id).collect();
            after = ids.last().copied().unwrap_or(0) as i64;
            // the records among them (the others are not in the history)
            let list: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
            let known: BTreeSet<Id> = self
                .src
                .borrow_mut()
                .conn()
                .query(&format!("SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text AND \"id\" IN ({})", list.join(", ")), &[Sql::Text(ent.clone())])?
                .iter()
                .map(|r| num(&r[0]) as Id)
                .collect();
            // a working set of its own: what the page's formulas read
            let shared: SharedSource = self.src.clone();
            let interp = &mut self.interp;
            interp.store = Store::over(shared);
            interp.store.untracked = true;
            interp.store.formula_overrides = self.by_id.clone();
            let recs: Vec<Id> = known.iter().copied().collect();
            {
                let mut s = self.src.borrow_mut();
                s.history_ids = ctx.wrong_rows.clone();
                s.history_ids.extend(recs.iter().copied());
            }
            interp.store.prefetch_as(ent, &recs);
            if computes {
                // the lines their formulas read, one query per collection
                for (child, back) in &lines {
                    interp.store.prefetch_find(child, back, &recs);
                }
                // and what all of them refer to (a line's order, its customer), level by level
                let loaded: Vec<Id> = interp.store.records.keys().copied().collect();
                interp.store.prefetch_refs(&loaded, 2);
            }
            for id in &recs {
                // the page's own formulas are computed
                if let Some(r) = interp.store.records.get_mut(id) {
                    r.formulas.clear();
                }
            }
            if first && !sets.is_empty() && !recs.is_empty() {
                // sets whose join table does not say what the history says
                let rl: Vec<String> = recs.iter().map(|i| i.to_string()).collect();
                for sm in &sets {
                    let mut held: HashMap<Id, BTreeSet<Id>> = HashMap::new();
                    for r in self.src.borrow_mut().conn().query(&format!("SELECT CAST(\"owner_id\" AS TEXT), CAST(\"target_id\" AS TEXT) FROM \"{}\" WHERE \"owner_id\" IN ({})", sm.table, rl.join(", ")), &[])? {
                        held.entry(num(&r[0]) as Id).or_default().insert(num(&r[1]) as Id);
                    }
                    for id in &recs {
                        let want: BTreeSet<Id> = interp
                            .store
                            .records
                            .get(id)
                            .and_then(|r| r.fields.get(&sm.field))
                            .map(quadra_runtime::store::ref_ids)
                            .unwrap_or_default()
                            .into_iter()
                            .collect();
                        if held.get(id).cloned().unwrap_or_default() != want {
                            out.patch.push(((ent.clone(), sm.field.clone()), *id));
                        }
                    }
                }
            }
            for r in &rows {
                let id = num(&r[0]) as Id;
                if !known.contains(&id) {
                    if first {
                        out.stored
                            .push(format!("table {}: row {id} is not in the history", t.name));
                    }
                    continue;
                }
                let rec = V::Ref {
                    ent: Rc::from(ent.as_str()),
                    id,
                };
                let mut i = 1;
                for fm in &t.fields {
                    if !first && !fm.formula {
                        i += fm.columns.len();
                        continue;
                    }
                    let v = if fm.formula {
                        interp.get(&rec, &fm.field, None, None).unwrap_or(V::None)
                    } else {
                        interp
                            .store
                            .records
                            .get(&id)
                            .and_then(|r| r.fields.get(&fm.field))
                            .cloned()
                            .unwrap_or(V::None)
                    };
                    let mut bad = false;
                    for c in &fm.columns {
                        let expected = encode(c, &v).as_text();
                        if !same(&r[i], &expected) {
                            bad = true;
                            let p = format!(
                                "{}.{} of {id}: the table has {}, the history says {}",
                                t.name,
                                names[i - 1],
                                r[i].as_deref().unwrap_or("NULL"),
                                expected.as_deref().unwrap_or("NULL")
                            );
                            if fm.formula {
                                out.problems.push(p);
                            } else {
                                out.stored.push(p);
                            }
                        }
                        i += 1;
                    }
                    if bad && fm.formula {
                        out.wrong
                            .push(((id, fm.field.clone()), quadra_runtime::codec::to_json(&v)));
                    } else if bad {
                        // a stored value the row does not have: lookups by it take the record
                        out.patch.push(((ent.clone(), fm.field.clone()), id));
                    }
                }
            }
            if let Some(e) = quadra_runtime::store::Source::error(&mut *self.src.borrow_mut()) {
                return Err(format!("database: {e}"));
            }
            let loaded = interp.store.records.len();
            if loaded > LOADED {
                size = (size * LOADED / loaded).max(1);
            } else if loaded < LOADED / 4 {
                size = (size * 2).min(page());
            }
        }
        // the working set of the last page goes now (not while the next unit starts)
        self.interp.store = Store::over(self.src.clone());
        self.size.insert(u.table, size);
        Ok(out)
    }
}

/// The collections of an entity computed from the other side that its formulas mention:
/// (entity of the lines, their field holding the record).
fn lines_read(model: &Model, ent: &str) -> Vec<(String, String)> {
    let Some(e) = model.entity(ent) else {
        return Vec::new();
    };
    let fields = model.all_fields(e);
    let mut mentioned: BTreeSet<String> = BTreeSet::new();
    for f in &fields {
        if let Some(x) = &f.formula {
            names(
                &serde_json::to_value(x).unwrap_or(serde_json::Value::Null),
                &mut mentioned,
            );
        }
    }
    let mut out = Vec::new();
    for f in &fields {
        if f.formula.is_some()
            || f.through.is_some()
            || !mentioned.contains(&f.name)
            || !crate::schema::is_inverse(model, e, f)
        {
            continue;
        }
        let Some(quadra_ir::Ty::List(inner) | quadra_ir::Ty::Set(inner)) = &f.ty else {
            continue;
        };
        let quadra_ir::Ty::Path { segments, .. } = inner.as_ref() else {
            continue;
        };
        let Some(child) = model.entity(&segments[0]) else {
            continue;
        };
        let back = f.via.clone().or_else(|| {
            child
                .fields
                .iter()
                .find(|x| {
                    let t = match &x.ty {
                        Some(quadra_ir::Ty::Optional(i)) => Some(i.as_ref()),
                        t => t.as_ref(),
                    };
                    matches!(t, Some(quadra_ir::Ty::Path { segments, .. }) if segments[0] == e.name || e.traits.contains(&segments[0]))
                })
                .map(|x| x.name.clone())
        });
        if let Some(b) = back {
            out.push((child.name.clone(), b));
        }
    }
    out
}

fn names(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    match v {
        serde_json::Value::String(s) => {
            out.insert(s.clone());
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| names(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| names(x, out)),
        _ => {}
    }
}
