//! The sequencer of a tenant: the one place where its commits are ordered.
//!
//! Transactions run anywhere (on any engine, in parallel), each on a snapshot of the database.
//! When one commits, its engine asks the sequencer for a transaction number (`reserve`): the
//! sequencer checks what the transaction read against what was committed after its snapshot
//! (the *window* of recent writes, in memory; the database for a snapshot older than the
//! window), and either refuses it (a conflict) or numbers it and records what it writes. The
//! engine then prepares the transaction's writes with that number (`Database::prepare`) and
//! hands them over (`submit`). The sequencer writes the prepared transactions in number order,
//! several at once: one database transaction and one fsync for a whole batch (group commit),
//! computes the hash chain, and answers each engine once its batch is durable: in the
//! database, and at the destinations the tenant's durability asks for (`DurabilitySink`).
//!
//! The sequencer never runs the interpreter: it compares sets of keys, numbers and writes.

use crate::conn::{Conn, Dialect, Sql};
use crate::db::CommitHasher;
use quadra_runtime::store::ReadSet;
use quadra_runtime::{CommitError, Conflict};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// What a transaction writes, as the window keeps it: enough to tell whether a later
/// transaction read any of it, and to say what.
#[derive(Clone, Debug, Default)]
pub struct WriteSet {
    /// stored fields written: (record, entity, field)
    pub fields: Vec<(u64, String, String)>,
    /// records whose rows change in any way (fields, metadata, materialized formulas)
    pub rows: Vec<(u64, String)>,
    /// records created
    pub created: Vec<(u64, String)>,
    /// ledger rows posted: ledger, and each column's value as a read set keys it
    pub ledger_rows: Vec<(String, Vec<(String, String)>)>,
    /// counters and their new value
    pub counters: Vec<(String, u64)>,
    /// the (user, key) of an API request with an idempotency key
    pub idempotency: Option<(u64, String)>,
    /// rollups changed by an addition: (record, field, amount)
    pub deltas: Vec<(u64, String, rust_decimal::Decimal)>,
    /// formula columns changed, by addition or not: (record, field)
    pub formula_fields: Vec<(u64, String)>,
    /// the values stored fields were written from and to: (entity, field, value key)
    pub field_values: Vec<(String, String, String)>,
}

/// An addition to a formula column: the column's value when the batch is written, plus
/// `amount` (an integer when `int`, a decimal otherwise).
#[derive(Clone, Debug)]
pub struct DeltaOp {
    pub table: String,
    pub id: u64,
    pub column: String,
    pub int: bool,
    pub amount: String,
    /// the placeholder of the column's type (`?numeric`, `?bigint`)
    pub param: String,
}

/// A rule on a rollup changed by delta, to hold on the latest value: `seen` (what the
/// transaction read) plus the additions committed since its snapshot plus `own` must compare
/// with `bound` as `op` says.
#[derive(Clone, Debug)]
pub struct GuardCheck {
    pub id: u64,
    pub field: String,
    pub op: quadra_ir::BinOp,
    pub seen: rust_decimal::Decimal,
    pub own: rust_decimal::Decimal,
    pub bound: rust_decimal::Decimal,
}

/// A transaction asking for its number.
pub struct Request {
    /// the committed transaction its working set was read at
    pub base: u64,
    /// the working set was loaded whole (not on demand): any commit after `base` conflicts
    pub whole: bool,
    pub reads: ReadSet,
    pub writes: WriteSet,
    pub guards: Vec<GuardCheck>,
    /// history rows and outbox events it will write (their numbers are reserved with it)
    pub history: u64,
    pub outbox: u64,
    pub at: String,
    pub user: Option<u64>,
    pub cause: String,
}

/// The numbers given to a transaction: the transaction's, and where its history rows and
/// outbox events start (exclusive).
#[derive(Clone, Copy, Debug)]
pub struct Reservation {
    pub tx: u64,
    pub seq: i64,
    pub outbox: i64,
}

/// The writes of a numbered transaction, ready to be sent (`Database::prepare`).
pub struct Prepared {
    pub statements: Vec<(String, Vec<Sql>)>,
    /// additions to formula columns, made after the statements
    pub deltas: Vec<DeltaOp>,
    /// the entries of its hash, in order (compact JSON)
    pub payload: Vec<String>,
    /// the next record id after those it used
    pub next_id: u64,
}

/// A batch made durable in the database: what a durability destination receives. The
/// statements, run in order on a copy of the database as of batch `first_number - 1`, bring it
/// to batch `number` and transaction `last_tx` (the commit rows and batch rows included). Several
/// batches can come as one (`first_number` to `number`); a batch that only wrote outside the hash
/// chain (`Sequencer::side`) has no transaction of its own (`first_tx = last_tx + 1`).
pub struct Batch<'a> {
    pub first_number: u64,
    pub number: u64,
    pub first_tx: u64,
    pub last_tx: u64,
    /// the hash of `last_tx`
    pub hash: &'a str,
    pub statements: &'a [(String, Vec<Sql>)],
}

/// Somewhere a batch is kept besides the tenant's database (object storage, another
/// machine). The sequencer calls it after the batch is committed in the database.
pub trait DurabilitySink: Send {
    fn persist(&mut self, batch: &Batch) -> Result<(), String>;
    /// The latest position this destination holds: (batch, transaction) (None: nothing, or it
    /// cannot tell). A destination not at the database's position (a crash between a commit and
    /// its upload, writes past the sequencer) gets a snapshot when the sequencer starts, so that
    /// what it holds always rebuilds the database.
    fn last(&mut self) -> Result<Option<(u64, u64)>, String> {
        Ok(None)
    }
    /// The segments it holds after batch `batch`, in order (to bring back a database that lost
    /// its last commits: a power cut with the batches already kept here).
    fn segments_after(&mut self, _batch: u64) -> Result<Vec<crate::segments::Segment>, String> {
        Ok(Vec::new())
    }
    /// Keeps a copy of the whole database at a position (a SQLite file at `file`). A destination
    /// that keeps no snapshots (only the batches, for someone else to read) ignores it.
    fn snapshot(&mut self, _batch: u64, _tx: u64, _file: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
    /// Drops what a snapshot it keeps at batch `batch` makes useless: the segments ending at or
    /// before it, the older snapshots.
    fn prune(&mut self, _batch: u64) -> Result<(), String> {
        Ok(())
    }
}

/// How a tenant's log stands (`Sequencer::health`), for the host's monitoring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// the last transaction written to the database
    pub written: u64,
    /// the last transaction the destinations keep (`written` without destinations)
    pub kept: u64,
    /// why the destinations count as down, while they do
    pub down: Option<String>,
    /// transactions refused since the start because too many were not kept
    pub refused: u64,
    /// whether it has destinations at all
    pub destinations: bool,
}

/// What `Health` is read from: kept up to date by the sequencer and its uploader, read by anyone
/// without asking their threads (a sequencer busy with a snapshot still answers at once).
#[derive(Default)]
struct HealthCell {
    written: std::sync::atomic::AtomicU64,
    kept: std::sync::atomic::AtomicU64,
    refused: std::sync::atomic::AtomicU64,
    destinations: std::sync::atomic::AtomicBool,
    down: std::sync::Mutex<Option<String>>,
}

/// A change outside the hash chain, run by the sequencer (`Sequencer::side`).
type SideFn = Box<dyn FnOnce(&mut Conn) -> Result<Box<dyn std::any::Any + Send>, String> + Send>;

/// A file of its own for a snapshot on its way to the destinations (many tenants, and their
/// sequencers, share a process).
fn snapshot_file() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("quadra-snapshot-{}-{n}.db", std::process::id()))
}

/// When a commit is confirmed to its client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// once the database has it (the destinations get it right after)
    Local,
    /// once one of the destinations has it as well
    Any,
    /// once every destination has it as well
    All,
}

pub struct Options {
    /// how long the window keeps a committed transaction (a snapshot older than that is
    /// checked against the database)
    pub keep: Duration,
    /// how long a batch waits for transactions still being prepared
    pub max_wait: Duration,
    /// transactions per batch at most
    pub max_batch: usize,
    /// a reserved transaction not submitted within this time is given up (its engine died)
    pub abandon_after: Duration,
    /// the most commits written and not yet kept by the destinations: past it new transactions
    /// are refused until the destinations catch up
    pub max_behind: u64,
    /// SQLite: a snapshot once the log since the last one is larger than this and than the
    /// database; the destinations then drop the segments it holds (`DurabilitySink::prune`)
    pub snapshot_bytes: u64,
    /// a snapshot also once this many batches went since the last one (and an eighth of the
    /// database's size): a log of many small batches does not pile up objects
    pub snapshot_segments: u64,
    pub destinations: Vec<Box<dyn DurabilitySink>>,
    pub rule: Rule,
    /// SQLite: the size the database may grow to (`conn::set_max_bytes`)
    pub max_bytes: Option<u64>,
    /// PostgreSQL: how long a statement may run (`conn::set_statement_timeout`)
    pub statement_timeout: Option<Duration>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            keep: Duration::from_secs(300),
            max_wait: Duration::from_millis(2),
            max_batch: 500,
            abandon_after: Duration::from_secs(60),
            max_behind: 20_000,
            snapshot_bytes: 64 << 20,
            snapshot_segments: 2000,
            destinations: Vec::new(),
            rule: Rule::Local,
            max_bytes: None,
            statement_timeout: None,
        }
    }
}

/// Where the result of a commit goes: a waiting engine, or whatever answers its client.
pub enum Responder {
    Chan(mpsc::Sender<Result<u64, CommitError>>),
    Call(Box<dyn FnOnce(Result<u64, CommitError>) + Send>),
}

impl Responder {
    pub fn send(self, r: Result<u64, CommitError>) -> Result<(), ()> {
        match self {
            Responder::Chan(c) => c.send(r).map_err(|_| ()),
            Responder::Call(f) => {
                f(r);
                Ok(())
            }
        }
    }
}

/// A commit handed over without waiting for it: its result arrives later (`then`).
#[derive(Default)]
pub struct Ticket {
    state: std::sync::Mutex<(
        Option<Result<u64, CommitError>>,
        Option<Box<dyn FnOnce(Result<u64, CommitError>) + Send>>,
    )>,
}

impl Ticket {
    fn complete(&self, r: Result<u64, CommitError>) {
        let mut s = self.state.lock().unwrap();
        match s.1.take() {
            Some(f) => {
                drop(s);
                f(r)
            }
            None => s.0 = Some(r),
        }
    }

    /// `f` gets the result: now if it is known, else when it arrives.
    pub fn then(&self, f: impl FnOnce(Result<u64, CommitError>) + Send + 'static) {
        let mut s = self.state.lock().unwrap();
        match s.0.take() {
            Some(r) => {
                drop(s);
                f(r)
            }
            None => s.1 = Some(Box::new(f)),
        }
    }
}

enum Msg {
    Reserve(Request, mpsc::Sender<Result<Reservation, CommitError>>),
    Submit(u64, Prepared, Responder),
    Reload(mpsc::Sender<()>),
    Cancel(u64),
    Durable(mpsc::Sender<u64>),
    Ids(u64, mpsc::Sender<u64>),
    Snapshot(Duration),
    Written(u64, Vec<u64>, mpsc::Sender<Option<bool>>),
    Side(
        SideFn,
        mpsc::Sender<Result<Box<dyn std::any::Any + Send>, String>>,
        Responder,
    ),
    Stop(mpsc::Sender<()>),
}

/// The handle engines use to reach their tenant's sequencer (it runs on a thread of its own).
#[derive(Clone)]
pub struct Sequencer {
    tx: mpsc::Sender<Msg>,
    health: std::sync::Arc<HealthCell>,
}

fn gone() -> String {
    "the sequencer of this project stopped".into()
}

impl Sequencer {
    /// Starts the sequencer of the database at `url`, which must be initialized.
    pub fn start(url: &str, options: Options) -> Result<Sequencer, String> {
        let (tx, rx) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let url = url.to_string();
        let health = std::sync::Arc::new(HealthCell::default());
        let cell = health.clone();
        std::thread::Builder::new()
            .name("quadra-sequencer".into())
            .spawn(move || match State::open(&url, options, cell) {
                Ok(mut s) => {
                    let _ = ready.send(Ok(()));
                    s.run(rx);
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        started.recv().map_err(|_| gone())??;
        Ok(Sequencer { tx, health })
    }

    /// Checks a transaction and numbers it, or refuses it.
    pub fn reserve(&self, r: Request) -> Result<Reservation, CommitError> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Msg::Reserve(r, tx))
            .map_err(|_| CommitError::Failed(gone()))?;
        rx.recv().map_err(|_| CommitError::Failed(gone()))?
    }

    /// Hands over the prepared writes of a reserved transaction and waits until they are
    /// durable; returns the transaction's number.
    pub fn submit(&self, tx: u64, p: Prepared) -> Result<u64, CommitError> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Msg::Submit(tx, p, Responder::Chan(reply)))
            .map_err(|_| CommitError::Failed(gone()))?;
        rx.recv().map_err(|_| CommitError::Failed(gone()))?
    }

    /// `submit` without waiting: `then` gets the result once the writes are durable.
    pub fn submit_then(
        &self,
        tx: u64,
        p: Prepared,
        then: Box<dyn FnOnce(Result<u64, CommitError>) + Send>,
    ) {
        if let Err(mpsc::SendError(Msg::Submit(_, _, r))) =
            self.tx.send(Msg::Submit(tx, p, Responder::Call(then)))
        {
            let _ = r.send(Err(CommitError::Failed(gone())));
        }
    }

    /// The database was written by someone else (a deploy, a migration): the sequencer
    /// writes what it holds, then reads where the database stands.
    pub fn reload(&self) {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Msg::Reload(reply)).is_ok() {
            let _ = rx.recv();
        }
    }

    /// `n` record ids for an engine; returns the first (0: the sequencer stopped). Kept in
    /// memory: the commit that uses them records the next free one.
    pub fn ids(&self, n: u64) -> Result<u64, String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Msg::Ids(n, reply)).map_err(|_| gone())?;
        rx.recv().map_err(|_| gone())
    }

    /// Whether a stored field of one of these records was written after `base`, from memory
    /// (None: `base` is older than what the sequencer remembers, ask the database).
    pub fn written_since(&self, base: u64, records: Vec<u64>) -> Option<bool> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Msg::Written(base, records, reply)).ok()?;
        rx.recv().ok().flatten()
    }

    /// The destinations should get a snapshot within `within`: the database changed in a way
    /// the batches do not carry (a deploy changed the schema; saved views, teams or tokens were
    /// written). Several requests before it is taken make one snapshot.
    pub fn snapshot_soon(&self, within: Duration) {
        let _ = self.tx.send(Msg::Snapshot(within));
    }

    /// Runs a change of the tables outside the hash chain (sign-in, tokens, saved views, teams)
    /// as a batch of its own, with no transaction: in the database and in the destinations in
    /// the order of the log, confirmed like a commit (under `Rule::All`, once kept). `f` runs on
    /// the sequencer's connection; its own `BEGIN`/`COMMIT` become part of the batch.
    pub fn side<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Conn) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let (value, value_rx) = mpsc::channel();
        let (kept, kept_rx) = mpsc::channel();
        let f: SideFn =
            Box::new(move |c| f(c).map(|v| Box::new(v) as Box<dyn std::any::Any + Send>));
        self.tx
            .send(Msg::Side(f, value, Responder::Chan(kept)))
            .map_err(|_| gone())?;
        let v = value_rx.recv().map_err(|_| gone())??;
        match kept_rx.recv() {
            Ok(Ok(_)) => {}
            Ok(Err(CommitError::Failed(e))) => return Err(e),
            Ok(Err(e)) => return Err(format!("{e:?}")),
            Err(_) => return Err(gone()),
        }
        v.downcast::<T>()
            .map(|v| *v)
            .map_err(|_| "the sequencer answered with another type".into())
    }

    /// How its log stands: what is written, what is kept, whether the destinations are down.
    pub fn health(&self) -> Option<Health> {
        use std::sync::atomic::Ordering::Relaxed;
        let h = &self.health;
        let written = h.written.load(Relaxed);
        let destinations = h.destinations.load(Relaxed);
        let kept = if destinations {
            h.kept.load(Relaxed).min(written)
        } else {
            written
        };
        Some(Health {
            written,
            kept,
            down: h.down.lock().unwrap().clone(),
            refused: h.refused.load(Relaxed),
            destinations,
        })
    }

    /// Gives up a reserved transaction (its writes could not be prepared).
    pub fn cancel(&self, tx: u64) {
        let _ = self.tx.send(Msg::Cancel(tx));
    }

    /// The last transaction confirmed: in the database and where the durability rule wants
    /// it. What is shown or sent outside (live events, the outbox) goes no further.
    pub fn durable(&self) -> Result<u64, String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Msg::Durable(reply)).map_err(|_| gone())?;
        rx.recv().map_err(|_| gone())
    }

    /// Writes what it holds and stops.
    pub fn stop(&self) {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Msg::Stop(reply)).is_ok() {
            let _ = rx.recv();
        }
    }
}

struct Pending {
    at: String,
    user: Option<u64>,
    cause: String,
    since: Instant,
    submitted: Option<(Prepared, Responder, Instant)>,
    cancelled: bool,
}

/// What the window remembers of a committed (or numbered) transaction.
struct Seen {
    tx: u64,
    at: String,
    user: Option<u64>,
    cause: String,
    writes: WriteSet,
    when: Instant,
}

/// The writes of the recent transactions, by what a read set may have read.
#[derive(Default)]
struct Window {
    /// transactions up to this one are not in the window
    floor: u64,
    seen: VecDeque<Seen>,
    fields: HashMap<u64, u64>,
    field_keys: HashMap<(String, String), u64>,
    rows: HashMap<u64, u64>,
    formula_fields: HashMap<(u64, String), u64>,
    field_values: HashMap<(String, String, String), u64>,
    entity_rows: HashMap<String, u64>,
    created: HashMap<String, u64>,
    ledgers: HashMap<String, u64>,
    counters: HashMap<String, (u64, u64)>,
    idempotency: HashSet<(u64, String)>,
}

fn raise<K: std::hash::Hash + Eq>(m: &mut HashMap<K, u64>, k: K, tx: u64) {
    let e = m.entry(k).or_insert(tx);
    *e = (*e).max(tx);
}

fn lower<K: std::hash::Hash + Eq>(m: &mut HashMap<K, u64>, k: &K, tx: u64) {
    if m.get(k) == Some(&tx) {
        m.remove(k);
    }
}

impl Window {
    fn add(&mut self, s: Seen) {
        let tx = s.tx;
        for (id, ent, field) in &s.writes.fields {
            raise(&mut self.fields, *id, tx);
            raise(&mut self.field_keys, (ent.clone(), field.clone()), tx);
        }
        for (id, ent) in &s.writes.rows {
            raise(&mut self.rows, *id, tx);
            raise(&mut self.entity_rows, ent.clone(), tx);
        }
        for k in &s.writes.formula_fields {
            raise(&mut self.formula_fields, k.clone(), tx);
        }
        for k in &s.writes.field_values {
            raise(&mut self.field_values, k.clone(), tx);
        }
        for (_, ent) in &s.writes.created {
            raise(&mut self.created, ent.clone(), tx);
        }
        for (ledger, _) in &s.writes.ledger_rows {
            raise(&mut self.ledgers, ledger.clone(), tx);
        }
        for (key, n) in &s.writes.counters {
            self.counters.insert(key.clone(), (tx, *n));
        }
        if let Some(k) = &s.writes.idempotency {
            self.idempotency.insert(k.clone());
        }
        self.seen.push_back(s);
    }

    /// Forgets the transactions older than `keep`, up to `durable`.
    fn prune(&mut self, keep: Duration, durable: u64) {
        while let Some(s) = self.seen.front() {
            if s.tx > durable || s.when.elapsed() < keep {
                break;
            }
            let s = self.seen.pop_front().unwrap();
            let tx = s.tx;
            for (id, ent, field) in &s.writes.fields {
                lower(&mut self.fields, id, tx);
                lower(&mut self.field_keys, &(ent.clone(), field.clone()), tx);
            }
            for (id, ent) in &s.writes.rows {
                lower(&mut self.rows, id, tx);
                lower(&mut self.entity_rows, ent, tx);
            }
            for k in &s.writes.formula_fields {
                lower(&mut self.formula_fields, k, tx);
            }
            for k in &s.writes.field_values {
                lower(&mut self.field_values, k, tx);
            }
            for (_, ent) in &s.writes.created {
                lower(&mut self.created, ent, tx);
            }
            for (ledger, _) in &s.writes.ledger_rows {
                lower(&mut self.ledgers, ledger, tx);
            }
            for (key, _) in &s.writes.counters {
                if self.counters.get(key).is_some_and(|(t, _)| *t == tx) {
                    self.counters.remove(key);
                }
            }
            if let Some(k) = &s.writes.idempotency {
                self.idempotency.remove(k);
            }
            self.floor = tx;
        }
    }

    /// Whether anything in the window after `base` touches what `reads` read: a quick test
    /// on the indexes (the details come from `describe`).
    fn touches(&self, base: u64, whole: bool, reads: &ReadSet) -> bool {
        let after = |t: Option<&u64>| t.is_some_and(|t| *t > base);
        if whole {
            return self.seen.back().is_some_and(|s| s.tx > base);
        }
        reads.records.iter().any(|id| after(self.fields.get(id)))
            || reads
                .formula_fields
                .iter()
                .any(|k| after(self.formula_fields.get(k)))
            || reads.formulas.iter().any(|id| {
                reads
                    .formula_fields
                    .range((*id, String::new())..)
                    .next()
                    .is_none_or(|(i, _)| i != id)
                    && after(self.rows.get(id))
            })
            || reads
                .formula_scans
                .iter()
                .any(|e| after(self.entity_rows.get(e)))
            || reads.scans.iter().any(|e| after(self.created.get(e)))
            || reads.finds.iter().any(|k| after(self.field_keys.get(k)))
            || reads
                .find_values
                .iter()
                .any(|k| after(self.field_values.get(k)))
            || reads
                .ledgers
                .iter()
                .any(|(l, _)| after(self.ledgers.get(l)))
            || reads.counters.iter().any(|(k, seen)| {
                self.counters
                    .get(k)
                    .is_some_and(|(t, n)| *t > base && n != seen)
            })
    }

    /// What each transaction after `base` changed that `reads` read, as the database's check
    /// says it (`Database::find_conflicts`).
    fn describe(
        &self,
        base: u64,
        whole: bool,
        reads: &ReadSet,
    ) -> BTreeMap<u64, (Vec<String>, &Seen)> {
        let mut out: BTreeMap<u64, (Vec<String>, &Seen)> = BTreeMap::new();
        for s in self.seen.iter().filter(|s| s.tx > base) {
            let mut what = Vec::new();
            if whole {
                what.push("data read by this transaction".to_string());
            } else {
                for (id, ent, field) in &s.writes.fields {
                    if reads.records.contains(id)
                        || reads.finds.contains(&(ent.clone(), field.clone()))
                    {
                        what.push(format!("{ent}#{id}.{field}"));
                    }
                }
                for k in &s.writes.field_values {
                    if reads.find_values.contains(k) {
                        what.push(format!("{} with {} = {}", k.0, k.1, k.2));
                    }
                }
                for (id, ent) in &s.writes.rows {
                    // a formula read by field: only a change of that formula
                    let by_field = reads
                        .formula_fields
                        .range((*id, String::new())..)
                        .next()
                        .is_some_and(|(i, _)| i == id);
                    let fields: Vec<&str> = s
                        .writes
                        .formula_fields
                        .iter()
                        .filter(|k| k.0 == *id && reads.formula_fields.contains(*k))
                        .map(|k| k.1.as_str())
                        .collect();
                    if by_field && !fields.is_empty() {
                        for f in fields {
                            what.push(format!("{ent}#{id}.{f} (computed)"));
                        }
                    } else if (!by_field && reads.formulas.contains(id))
                        || reads.formula_scans.contains(ent)
                    {
                        what.push(format!("{ent}#{id} (its computed values)"));
                    }
                }
                for (id, ent) in &s.writes.created {
                    if reads.scans.contains(ent) {
                        what.push(format!("new {ent}#{id}"));
                    }
                }
                for (ledger, cols) in &s.writes.ledger_rows {
                    for (l, filter) in &reads.ledgers {
                        if l != ledger {
                            continue;
                        }
                        match filter {
                            None => what.push(format!("{ledger} rows")),
                            Some((col, key)) if cols.iter().any(|(c, v)| c == col && v == key) => {
                                what.push(format!("{ledger} rows with {col} = {key}"))
                            }
                            Some(_) => {}
                        }
                    }
                }
                for (key, n) in &s.writes.counters {
                    if reads.counters.get(key).is_some_and(|seen| seen != n) {
                        what.push(format!("counter {key}"));
                    }
                }
            }
            if !what.is_empty() {
                what.sort();
                what.dedup();
                out.insert(s.tx, (what, s));
            }
        }
        out
    }
}

/// Why a batch could not be written.
enum Failure {
    /// someone else committed since the last batch
    Foreign,
    Db(String),
}

struct State {
    conn: Conn,
    /// the last transaction numbered
    head: u64,
    /// the last transaction committed in the database
    durable: u64,
    /// the last transaction confirmed (in the database and where the durability rule wants
    /// it): what may be shown or sent outside
    acked: u64,
    seq: i64,
    outbox: i64,
    prev_hash: String,
    next_id: u64,
    /// the last id given to an engine
    ids_given: u64,
    batch: u64,
    window: Window,
    pending: BTreeMap<u64, Pending>,
    options: Options,
    /// the logins of users, by id (for the conflicts' messages)
    logins: HashMap<u64, Option<String>>,
    /// (QUADRA_SEQ_STATS) batches, transactions, time writing, time waiting for submissions
    stats: Option<(u64, u64, Duration, Duration, Instant)>,
    /// (QUADRA_SEQ_STATS) why transactions were refused, by kind of thing read
    refusals: BTreeMap<String, u64>,
    /// held while the sequencer runs: no other writer (released before it says it stopped)
    lock: Option<crate::writer::WriterLock>,
    /// with durability destinations: the thread that keeps the batches there and confirms them,
    /// while this one goes on writing the next batches to the database
    uploader: Option<Uploader>,
    /// when a snapshot is due for the destinations
    snapshot_due: Option<Instant>,
    /// the size of the batches sent to the destinations since their last snapshot, and how many
    log_bytes: u64,
    log_segments: u64,
    /// transactions refused because too many were not kept by the destinations
    refused: u64,
    /// numbered and never written (given up, or refused by the database): their deltas count
    /// for no rule on a total
    void: HashSet<u64>,
    /// what `Sequencer::health` reads
    health: std::sync::Arc<HealthCell>,
    url: String,
}

fn num(s: &Option<String>) -> i64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

impl State {
    fn open(
        url: &str,
        options: Options,
        health: std::sync::Arc<HealthCell>,
    ) -> Result<State, String> {
        let lock = crate::writer::acquire(url)?;
        crate::conn::set_max_bytes(options.max_bytes);
        crate::conn::set_statement_timeout(options.statement_timeout);
        let mut conn = Conn::open(url)?;
        // (a database older than the sequencer)
        conn.batch("CREATE TABLE IF NOT EXISTS \"_quadra_batches\" (\"number\" BIGINT PRIMARY KEY, \"first_tx\" BIGINT NOT NULL, \"last_tx\" BIGINT NOT NULL, \"hash\" TEXT NOT NULL)")?;
        let mut s = State {
            conn,
            head: 0,
            durable: 0,
            acked: 0,
            seq: 0,
            outbox: 0,
            prev_hash: "0".repeat(64),
            next_id: 0,
            ids_given: 0,
            batch: 0,
            window: Window::default(),
            pending: BTreeMap::new(),
            options,
            logins: HashMap::new(),
            stats: std::env::var_os("QUADRA_SEQ_STATS")
                .map(|_| (0, 0, Duration::ZERO, Duration::ZERO, Instant::now())),
            refusals: BTreeMap::new(),
            lock: Some(lock),
            uploader: None,
            snapshot_due: None,
            log_bytes: 0,
            log_segments: 0,
            refused: 0,
            void: HashSet::new(),
            health,
            url: url.to_string(),
        };
        s.reload()?;
        let mut destinations = std::mem::take(&mut s.options.destinations);
        // with the batches kept elsewhere before they are confirmed, the database itself need not
        // sync at every commit: SQLite in WAL then syncs at checkpoints (the log is the durability)
        if !destinations.is_empty()
            && s.conn.dialect() == Dialect::Sqlite
            && s.options.rule != Rule::Local
        {
            s.conn.batch("PRAGMA synchronous = NORMAL")?;
        }
        // a destination ahead of the database (the machine lost its last commits, already kept
        // there and confirmed): the database gets them back before anything else
        for d in destinations.iter_mut() {
            if let Ok(Some((b, _))) = d.last() {
                if b > s.batch {
                    let segments = d.segments_after(s.batch)?;
                    let at = crate::segments::apply(&mut s.conn, &segments)?;
                    eprintln!("sequencer: the database was at batch {} (transaction {}), brought back to transaction {at} from a destination", s.batch, s.durable);
                    s.reload()?;
                }
            }
        }
        // a destination behind the database (the process died between a commit and its upload,
        // or it is new): a snapshot of now, so that it can rebuild the database again
        // (PostgreSQL: no snapshots, it has its own backups)
        let sqlite = s.conn.dialect() == Dialect::Sqlite;
        for d in destinations.iter_mut().filter(|_| sqlite) {
            // (one that cannot say what it holds is taken as behind: a snapshot costs less than a log
            // that cannot rebuild the database)
            match d.last() {
                Ok(Some(p)) if p == (s.batch, s.durable) => {}
                answer => {
                    if let Err(e) = answer {
                        eprintln!("sequencer: a destination cannot say what it holds ({e}): it gets a snapshot");
                    }
                    let file = snapshot_file();
                    let r = crate::segments::snapshot(url, &file)
                        .and_then(|(b, t)| d.snapshot(b, t, &file));
                    let _ = std::fs::remove_file(&file);
                    if let Err(e) = r {
                        // what it would keep from now on could not rebuild the database
                        if s.options.rule != Rule::Local {
                            return Err(format!(
                                "a destination is behind and could not get a snapshot: {e}"
                            ));
                        }
                        eprintln!("sequencer: a destination is behind and could not get a snapshot ({e}): another in a minute");
                        s.snapshot_due = Some(Instant::now() + Duration::from_secs(60));
                    }
                }
            }
        }
        if !destinations.is_empty() {
            s.health
                .destinations
                .store(true, std::sync::atomic::Ordering::Relaxed);
            s.uploader = Some(Uploader::start(
                destinations,
                s.options.rule,
                s.acked,
                s.health.clone(),
            )?);
        }
        Ok(s)
    }

    /// Reads where the database stands; the window starts empty there.
    fn reload(&mut self) -> Result<(), String> {
        let r = self.conn.query(
            "SELECT CAST((SELECT MAX(\"tx\") FROM \"_quadra_commits\") AS TEXT), CAST((SELECT MAX(\"seq\") FROM \"_quadra_history\") AS TEXT), (SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1), CAST((SELECT MAX(\"id\") FROM \"_quadra_outbox\") AS TEXT), (SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'state'), CAST((SELECT MAX(\"number\") FROM \"_quadra_batches\") AS TEXT)",
            &[],
        )?;
        let row = r.into_iter().next().unwrap_or_default();
        let at = |i: usize| row.get(i).cloned().flatten();
        let written = num(&at(0)) as u64;
        // numbers given before a restart are never given again: an engine late with its writes
        // for one of them must not land on a new transaction (the batch after covers the gap)
        self.head = self.head.max(written);
        self.durable = written;
        self.health
            .written
            .store(self.durable, std::sync::atomic::Ordering::Relaxed);
        self.acked = written;
        self.seq = num(&at(1));
        self.prev_hash = at(2).unwrap_or_else(|| "0".repeat(64));
        self.outbox = num(&at(3));
        self.next_id = at(4)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|s| s["next_id"].as_u64())
            .unwrap_or(0);
        // ids reserved by engines before (in the database): never given twice
        let reserved = self.conn.query(
            "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'id_reserved'",
            &[],
        )?;
        let reserved = reserved
            .first()
            .and_then(|r| r[0].as_deref())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        self.ids_given = self.ids_given.max(reserved).max(self.next_id);
        self.batch = num(&at(5)) as u64;
        self.window = Window {
            floor: self.durable,
            ..Default::default()
        };
        Ok(())
    }

    fn run(&mut self, rx: mpsc::Receiver<Msg>) {
        loop {
            let msg = match self.next_wait() {
                None => rx.recv().ok(),
                Some(d) if d.is_zero() => match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(TryRecvError::Empty) => {
                        self.guarded(|s| s.flush());
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => None,
                },
                Some(d) => match rx.recv_timeout(d) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => {
                        self.guarded(|s| s.flush());
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                },
            };
            let Some(msg) = msg else {
                self.flush_all();
                if let Some(u) = self.uploader.take() {
                    u.finish();
                }
                return;
            };
            if let Some(stopped) = self.guarded(|s| s.handle(msg)).flatten() {
                self.flush_all();
                // a snapshot asked for and not taken yet (after a deploy, writes past the
                // sequencer): before the end, or the destinations could not rebuild what is now
                if self.snapshot_due.is_some() {
                    self.take_snapshot();
                }
                if let Some(u) = self.uploader.take() {
                    u.finish();
                }
                // the WAL written back into the file before anyone takes the file (a hibernation
                // uploads it whole): checked, and tried again while a reader holds it
                if self.conn.dialect() == Dialect::Sqlite {
                    // (each try waits for readers up to the connection's busy timeout)
                    for attempt in 0..3 {
                        match self.conn.query("PRAGMA wal_checkpoint(TRUNCATE)", &[]) {
                            Ok(r) if r.first().and_then(|r| r[0].as_deref()) == Some("0") => break,
                            _ if attempt == 2 => eprintln!("sequencer: the WAL could not be written back into the file at the stop"),
                            _ => std::thread::sleep(Duration::from_millis(100)),
                        }
                    }
                }
                self.lock = None;
                let _ = stopped.send(());
                return;
            }
        }
    }

    /// A bug met on the sequencer's thread (in the code of a change outside the chain, in
    /// conflict checks) does not stop it: what was open is rolled back, the transactions in
    /// line are refused and the state read again from the database, with a snapshot for the
    /// destinations (a batch written and not yet handed over would be missing from the log).
    fn guarded<T>(&mut self, f: impl FnOnce(&mut State) -> T) -> Option<T> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(v) => Some(v),
            Err(panic) => {
                let why = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                eprintln!("sequencer: an internal error ({why}): the state is read again from the database");
                crate::conn::side_end();
                let _ = self.conn.batch("ROLLBACK");
                self.restart("an internal error of the sequencer: try again");
                None
            }
        }
    }

    /// How long to wait for more messages before writing a batch: None, nothing to write
    /// (wait for a message); zero, write once the messages already sent are handled.
    fn next_wait(&mut self) -> Option<Duration> {
        self.abandon_stale();
        if self.pending.is_empty() {
            if let Some(due) = self.snapshot_due {
                if due <= Instant::now() {
                    self.take_snapshot();
                } else {
                    return Some(due - Instant::now());
                }
            }
        }
        let mut ready = 0;
        let mut first: Option<Instant> = None;
        for p in self.pending.values() {
            match &p.submitted {
                Some((_, _, at)) => {
                    ready += 1;
                    first = Some(first.map_or(*at, |f: Instant| f.min(*at)));
                }
                None if p.cancelled => {}
                None => break,
            }
        }
        let Some(first) = first else {
            // the first in line is still being prepared: woken when it is given up at the latest
            return self
                .pending
                .values()
                .filter(|p| p.submitted.is_none() && !p.cancelled)
                .map(|p| self.options.abandon_after.saturating_sub(p.since.elapsed()))
                .min();
        };
        if ready >= self.options.max_batch {
            return Some(Duration::ZERO);
        }
        // transactions numbered but still being prepared may join the batch
        let preparing = self
            .pending
            .values()
            .any(|p| p.submitted.is_none() && !p.cancelled);
        if !preparing {
            return Some(Duration::ZERO);
        }
        Some(self.options.max_wait.saturating_sub(first.elapsed()))
    }

    /// Reserved transactions whose engine never came back are given up.
    fn abandon_stale(&mut self) {
        let limit = self.options.abandon_after;
        for p in self.pending.values_mut() {
            if p.submitted.is_none() && !p.cancelled && p.since.elapsed() > limit {
                p.cancelled = true;
            }
        }
    }

    fn handle(&mut self, msg: Msg) -> Option<mpsc::Sender<()>> {
        match msg {
            Msg::Reserve(r, reply) => {
                let res = self.reserve(r);
                let _ = reply.send(res);
            }
            Msg::Submit(tx, p, reply) => match self.pending.get_mut(&tx) {
                Some(pending) if !pending.cancelled => {
                    pending.submitted = Some((p, reply, Instant::now()))
                }
                // given up (too slow, or the sequencer started again): the engine runs it again
                _ => {
                    let _ = reply.send(Err(CommitError::Conflict(vec![Conflict {
                        tx,
                        at: String::new(),
                        user: None,
                        cause: "given up before its writes arrived".into(),
                        what: vec!["this transaction".into()],
                    }])));
                }
            },
            Msg::Reload(reply) => {
                self.flush();
                self.restart("the database was changed by a deploy or a migration: try again");
                // the batches after it are of the new schema: the destinations need it as it is now
                self.snapshot_due = Some(Instant::now());
                let _ = reply.send(());
            }
            Msg::Cancel(tx) => {
                if let Some(p) = self.pending.get_mut(&tx) {
                    p.cancelled = true;
                }
            }
            Msg::Durable(reply) => {
                let acked = self.uploader.as_ref().map_or(self.acked, |u| {
                    u.acked.load(std::sync::atomic::Ordering::SeqCst)
                });
                let _ = reply.send(acked);
            }
            Msg::Written(base, records, reply) => {
                let known = base >= self.window.floor;
                let _ = reply.send(known.then(|| {
                    records
                        .iter()
                        .any(|id| self.window.fields.get(id).is_some_and(|t| *t > base))
                }));
            }
            Msg::Side(f, value, kept) => {
                // in the order of the log: the transactions ready before it are written first
                self.flush();
                self.side(f, value, kept);
            }
            Msg::Snapshot(within) => {
                let at = Instant::now() + within;
                self.snapshot_due = Some(self.snapshot_due.map_or(at, |d| d.min(at)));
            }
            Msg::Ids(n, reply) => {
                let first = self.next_id.max(self.ids_given) + 1;
                self.ids_given = first + n - 1;
                let _ = reply.send(first);
            }
            Msg::Stop(reply) => return Some(reply),
        }
        None
    }

    fn reserve(&mut self, r: Request) -> Result<Reservation, CommitError> {
        let f = CommitError::Failed;
        if let Some(u) = &self.uploader {
            // the destinations unreachable for long: no more commits only this machine holds
            let kept = u.acked.load(std::sync::atomic::Ordering::SeqCst);
            if self.durable.saturating_sub(kept) > self.options.max_behind {
                self.refused += 1;
                self.health
                    .refused
                    .store(self.refused, std::sync::atomic::Ordering::Relaxed);
                return Err(f(format!(
                    "{} commits are not yet kept elsewhere: try again later",
                    self.durable - kept
                )));
            }
        }
        if r.base > self.head {
            // the database moved past the sequencer (another writer): start again from it
            self.restart("the database was changed by another writer: try again");
            if r.base > self.head {
                return Err(f(format!(
                    "base {} is ahead of the latest commit ({})",
                    r.base, self.head
                )));
            }
        }
        if let Some(k) = &r.writes.idempotency {
            if self.window.idempotency.contains(k) {
                return Err(CommitError::Conflict(vec![Conflict {
                    tx: self.head,
                    at: String::new(),
                    user: None,
                    cause: "the same request (its idempotency key) is being saved".into(),
                    what: vec![format!("idempotency key {}", k.1)],
                }]));
            }
        }
        let mut conflicts = Vec::new();
        // a snapshot older than the window: the database says what changed up to now
        if r.base < self.window.floor {
            let found = if r.whole {
                crate::db::commits_after(&mut self.conn, r.base)
            } else {
                crate::db::find_conflicts(&mut self.conn, r.base, &r.reads)
            };
            conflicts.extend(found.map_err(f)?);
        }
        if self.window.touches(r.base, r.whole, &r.reads) {
            let described: Vec<(u64, Vec<String>, String, Option<u64>, String)> = self
                .window
                .describe(r.base, r.whole, &r.reads)
                .into_iter()
                .map(|(tx, (what, s))| (tx, what, s.at.clone(), s.user, s.cause.clone()))
                .collect();
            for (tx, what, at, user, cause) in described {
                if conflicts.iter().any(|c: &Conflict| c.tx == tx) {
                    continue;
                }
                let user = user.and_then(|u| self.login(u));
                conflicts.push(Conflict {
                    tx,
                    at,
                    user,
                    cause,
                    what,
                });
            }
        }
        if !conflicts.is_empty() {
            if self.stats.is_some() {
                for c in &conflicts {
                    for w in &c.what {
                        let kind: String = w
                            .chars()
                            .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
                            .collect();
                        *self
                            .refusals
                            .entry(
                                kind.replace("##", "#")
                                    .replace("##", "#")
                                    .replace("##", "#"),
                            )
                            .or_default() += 1;
                    }
                }
            }
            conflicts.sort_by_key(|c| c.tx);
            return Err(CommitError::Conflict(conflicts));
        }
        if let Some(c) = self.broken_guard(&r) {
            return Err(CommitError::Conflict(vec![c]));
        }
        self.head += 1;
        let tx = self.head;
        let res = Reservation {
            tx,
            seq: self.seq,
            outbox: self.outbox,
        };
        self.seq += r.history as i64;
        self.outbox += r.outbox as i64;
        self.window.add(Seen {
            tx,
            at: r.at.clone(),
            user: r.user,
            cause: r.cause.clone(),
            writes: r.writes,
            when: Instant::now(),
        });
        self.pending.insert(
            tx,
            Pending {
                at: r.at,
                user: r.user,
                cause: r.cause,
                since: Instant::now(),
                submitted: None,
                cancelled: false,
            },
        );
        Ok(res)
    }

    /// A rule on a rollup that no longer holds with what was committed since the snapshot
    /// (or cannot be told from the window): refused, and run again on the latest data.
    fn broken_guard(&self, r: &Request) -> Option<Conflict> {
        for g in &r.guards {
            let refused = |why: &str| Conflict {
                tx: self.head,
                at: String::new(),
                user: None,
                cause: why.to_string(),
                what: vec![format!("#{}.{}", g.id, g.field)],
            };
            if r.base < self.window.floor {
                return Some(refused("a rule on a total read long ago"));
            }
            let mut now = g.seen + g.own;
            // a lower bound (>=, >) can be broken by what takes away, an upper one by what adds
            let lower = !matches!(g.op, quadra_ir::BinOp::Lt | quadra_ir::BinOp::Le);
            for s in self
                .window
                .seen
                .iter()
                .filter(|s| s.tx > r.base && !self.void.contains(&s.tx))
            {
                if !s.writes.rows.iter().any(|(id, _)| *id == g.id) {
                    continue;
                }
                match s
                    .writes
                    .deltas
                    .iter()
                    .find(|(id, f, _)| *id == g.id && *f == g.field)
                {
                    // written: as it is; still to be written (it may never be): only in the
                    // direction that could break the rule
                    Some((_, _, d)) if s.tx <= self.durable => now += *d,
                    Some((_, _, d)) => {
                        if (lower && d.is_sign_negative()) || (!lower && d.is_sign_positive()) {
                            now += *d;
                        }
                    }
                    None => return Some(refused("the record changed since it was read")),
                }
            }
            let holds = match g.op {
                quadra_ir::BinOp::Lt => now < g.bound,
                quadra_ir::BinOp::Le => now <= g.bound,
                quadra_ir::BinOp::Gt => now > g.bound,
                _ => now >= g.bound,
            };
            if !holds {
                return Some(refused(
                    "the rule on this total no longer holds with what others committed",
                ));
            }
        }
        None
    }

    fn login(&mut self, user: u64) -> Option<String> {
        if let Some(l) = self.logins.get(&user) {
            return l.clone();
        }
        let l = self
            .conn
            .query("SELECT \"value\" FROM \"_quadra_history\" WHERE \"record_id\" = ?bigint AND \"field\" = 'login' ORDER BY \"_quadra_history\".\"seq\" DESC", &[Sql::Int(user as i64)])
            .ok()
            .and_then(|r| r.first().and_then(|x| x[0].clone()))
            .map(|v| crate::codec::from_json(&serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)).to_string());
        self.logins.insert(user, l.clone());
        l
    }

    /// Writes every transaction ready, batch after batch.
    fn flush_all(&mut self) {
        loop {
            let before = self.pending.len();
            self.flush();
            if self.pending.len() == before {
                break;
            }
        }
        // what is left will never be submitted now
        for (_, p) in std::mem::take(&mut self.pending) {
            if let Some((_, reply, _)) = p.submitted {
                let _ = reply.send(Err(CommitError::Failed(gone())));
            }
        }
    }

    /// Starts again from where the database stands: the transactions numbered and not written
    /// are refused as conflicts (their engines run them again on the new state).
    fn restart(&mut self, why: &str) {
        let refused = Conflict {
            tx: self.durable,
            at: String::new(),
            user: None,
            cause: why.to_string(),
            what: vec!["the project's database".into()],
        };
        for (_, p) in std::mem::take(&mut self.pending) {
            if let Some((_, reply, _)) = p.submitted {
                let _ = reply.send(Err(CommitError::Conflict(vec![refused.clone()])));
            }
        }
        if let Err(e) = self.reload() {
            eprintln!("sequencer: cannot read the database again: {e}");
        }
        // what was written past the sequencer is in no batch: the destinations need a snapshot
        self.snapshot_due = Some(Instant::now());
    }

    /// Writes the transactions ready at the front (submitted or given up, in number order) as
    /// one batch.
    fn flush(&mut self) {
        let mut items: Vec<(u64, Pending)> = Vec::new();
        while let Some((&tx, p)) = self.pending.iter().next() {
            if (p.submitted.is_none() && !p.cancelled) || items.len() >= self.options.max_batch {
                break;
            }
            items.push((tx, self.pending.remove(&tx).unwrap()));
        }
        for (tx, p) in &items {
            if p.submitted.is_none() {
                self.void.insert(*tx);
            }
        }
        let submitted: Vec<(u64, Pending)> = items
            .into_iter()
            .filter(|(_, p)| p.submitted.is_some())
            .collect();
        if submitted.is_empty() {
            return;
        }
        let clock = Instant::now();
        let waited: Duration = submitted
            .iter()
            .filter_map(|(_, p)| p.submitted.as_ref().map(|(_, _, at)| at.elapsed()))
            .max()
            .unwrap_or_default();
        let n = submitted.len() as u64;
        // a batch covers every number after the last one written, those given up included: the
        // log has no gap where a reserved transaction was never written
        let from = self.durable + 1;
        let results = match self.write(&submitted) {
            Ok(executed) => {
                let first = from;
                let last = submitted.last().unwrap().0;
                self.durable = self.durable.max(last);
                self.health
                    .written
                    .store(self.durable, std::sync::atomic::Ordering::Relaxed);
                if self.uploader.is_some() && self.options.rule != Rule::Local {
                    // confirmed by the uploader once the batch is kept elsewhere too
                    let replies = submitted
                        .into_iter()
                        .filter_map(|(tx, p)| p.submitted.map(|(_, reply, _)| (tx, reply)))
                        .collect();
                    self.upload(first, last, executed, replies);
                    self.after_flush(clock, waited, n);
                    return;
                }
                // the fast class: confirmed now, kept elsewhere right after
                if self.uploader.is_some() {
                    self.upload(first, last, executed, Vec::new());
                }
                self.acked = self.acked.max(last);
                submitted.iter().map(|(tx, _)| Ok(*tx)).collect::<Vec<_>>()
            }
            Err(Failure::Foreign) => {
                // someone else committed: these and every other numbered transaction run again
                for (tx, p) in submitted {
                    self.pending.insert(tx, p);
                }
                self.restart("the database was changed by another writer: try again");
                return;
            }
            // one by one: a transaction the database refuses fails alone
            Err(Failure::Db(_)) => {
                self.write_one_by_one(submitted);
                self.after_flush(clock, waited, n);
                return;
            }
        };
        for ((_, p), res) in submitted.into_iter().zip(results) {
            if let Some((_, reply, _)) = p.submitted {
                let _ = reply.send(res);
            }
        }
        self.after_flush(clock, waited, n);
    }

    fn after_flush(&mut self, clock: Instant, waited: Duration, n: u64) {
        if let Some((batches, txs, writing, waiting, since)) = &mut self.stats {
            *batches += 1;
            *txs += n;
            *writing += clock.elapsed();
            *waiting += waited.saturating_sub(clock.elapsed());
            if since.elapsed() > Duration::from_secs(5) {
                eprintln!(
                    "sequencer: {batches} batches, {:.1} tx/batch, writing {:.2} ms/batch, waited {:.2} ms/batch, busy {:.0}%",
                    *txs as f64 / *batches as f64,
                    writing.as_secs_f64() * 1000.0 / *batches as f64,
                    waiting.as_secs_f64() * 1000.0 / *batches as f64,
                    writing.as_secs_f64() * 100.0 / since.elapsed().as_secs_f64()
                );
                let mut top: Vec<(&String, &u64)> = self.refusals.iter().collect();
                top.sort_by(|a, b| b.1.cmp(a.1));
                for (k, n) in top.into_iter().take(8) {
                    eprintln!("sequencer:   refused {n} × {k}");
                }
                self.refusals.clear();
                *batches = 0;
                *txs = 0;
                *writing = Duration::ZERO;
                *waiting = Duration::ZERO;
                *since = Instant::now();
            }
        }
        self.window.prune(self.options.keep, self.durable);
        let floor = self.window.floor;
        self.void.retain(|t| *t >= floor);
    }

    /// Writes the transactions one by one and answers each: a refused one fails alone, one
    /// that went through is confirmed now (`Rule::Local`) or by the uploader once kept elsewhere,
    /// never by waiting here (the sequencer thread keeps serving).
    fn write_one_by_one(&mut self, items: Vec<(u64, Pending)>) {
        for item in items {
            let from = self.durable + 1;
            let res = self.write(std::slice::from_ref(&item));
            let (tx, p) = item;
            let Some((_, reply, _)) = p.submitted else {
                continue;
            };
            let res = match res {
                Ok(executed) => {
                    self.durable = self.durable.max(tx);
                    self.health
                        .written
                        .store(self.durable, std::sync::atomic::Ordering::Relaxed);
                    if self.uploader.is_some() && self.options.rule != Rule::Local {
                        self.upload(from, tx, executed, vec![(tx, reply)]);
                        continue;
                    }
                    if self.uploader.is_some() {
                        self.upload(from, tx, executed, Vec::new());
                    }
                    self.acked = self.acked.max(tx);
                    Ok(tx)
                }
                Err(Failure::Db(e)) => {
                    self.void.insert(tx);
                    Err(CommitError::Failed(e))
                }
                Err(Failure::Foreign) => Err(CommitError::Failed(
                    "the database was changed by another writer".into(),
                )),
            };
            let _ = reply.send(res);
        }
    }

    /// A change outside the hash chain as a batch of its own (`Sequencer::side`): `f` and the
    /// batch's row in one transaction (the `BEGIN`/`COMMIT` of `f` are folded into it), then the
    /// statements to the destinations. No transaction number: `first_tx = last_tx + 1`.
    fn side(
        &mut self,
        f: SideFn,
        value: mpsc::Sender<Result<Box<dyn std::any::Any + Send>, String>>,
        kept: Responder,
    ) {
        let sqlite = self.conn.dialect() == Dialect::Sqlite;
        let began = self
            .conn
            .batch(if sqlite { "BEGIN IMMEDIATE" } else { "BEGIN" });
        if let Err(e) = began {
            let _ = value.send(Err(e));
            let _ = kept.send(Ok(self.durable));
            return;
        }
        crate::conn::side_begin();
        let r = f(&mut self.conn);
        let side = crate::conn::side_end();
        let v = match r {
            Ok(v) if !side.rolled_back && !side.statements.is_empty() => v,
            other => {
                // failed, rolled back by `f`, or nothing written: nothing to keep
                let _ = self.conn.batch(if other.is_ok() && !side.rolled_back {
                    "COMMIT"
                } else {
                    "ROLLBACK"
                });
                let _ = value.send(other);
                let _ = kept.send(Ok(self.durable));
                return;
            }
        };
        let (first, last) = (self.durable + 1, self.durable);
        let row = (
            "INSERT INTO \"_quadra_batches\" (\"number\", \"first_tx\", \"last_tx\", \"hash\") VALUES (?bigint, ?bigint, ?bigint, ?text)".to_string(),
            vec![Sql::Int(self.batch as i64 + 1), Sql::Int(first as i64), Sql::Int(last as i64), Sql::Text(self.prev_hash.clone())],
        );
        let mut statements = side.statements;
        let written = self
            .conn
            .exec(&row.0, &row.1)
            .and_then(|_| self.conn.batch("COMMIT"));
        if let Err(e) = written {
            let _ = self.conn.batch("ROLLBACK");
            let _ = value.send(Err(e));
            let _ = kept.send(Ok(self.durable));
            return;
        }
        statements.push(row);
        self.batch += 1;
        let _ = value.send(Ok(v));
        if self.uploader.is_some() && self.options.rule != Rule::Local {
            self.upload(first, last, statements, vec![(self.durable, kept)]);
        } else {
            if self.uploader.is_some() {
                self.upload(first, last, statements, Vec::new());
            }
            let _ = kept.send(Ok(self.durable));
        }
    }

    /// A snapshot of the database now, handed to the uploader after the batches before it.
    fn take_snapshot(&mut self) {
        self.snapshot_due = None;
        self.log_bytes = 0;
        self.log_segments = 0;
        let Some(up) = &self.uploader else { return };
        let file = snapshot_file();
        match crate::segments::snapshot(&self.url, &file) {
            Ok((batch, tx)) => {
                let u = Upload {
                    snapshot: Some((batch, tx, file)),
                    number: batch,
                    first_tx: tx,
                    last_tx: tx,
                    hash: self.prev_hash.clone(),
                    statements: Vec::new(),
                    replies: Vec::new(),
                };
                let _ = up.tx.send(u);
            }
            Err(e) => eprintln!("sequencer: cannot take a snapshot: {e}"),
        }
    }

    /// Hands a batch written to the database to the uploader, with the clients to confirm.
    fn upload(
        &mut self,
        first: u64,
        last: u64,
        statements: Vec<(String, Vec<Sql>)>,
        replies: Vec<(u64, Responder)>,
    ) {
        // the log since the last snapshot as large as the database (and past `snapshot_bytes`),
        // or many segments and an eighth of its size: a new snapshot, after which the
        // destinations drop what it holds (few objects stay, a restore reads few)
        self.log_bytes += statements
            .iter()
            .map(|(sql, ps)| {
                sql.len() as u64
                    + ps.iter()
                        .map(|p| match p {
                            Sql::Text(t) => t.len() as u64,
                            _ => 8,
                        })
                        .sum::<u64>()
            })
            .sum::<u64>();
        self.log_segments += 1;
        let many = self.log_segments > self.options.snapshot_segments;
        if (self.log_bytes > self.options.snapshot_bytes || many)
            && self.snapshot_due.is_none()
            && self.conn.dialect() == Dialect::Sqlite
        {
            let size = self.conn.query("SELECT CAST(page_count * page_size AS TEXT) FROM pragma_page_count(), pragma_page_size()", &[]).ok().and_then(|r| r.into_iter().next()).and_then(|r| r[0].as_deref().and_then(|v| v.parse::<u64>().ok())).unwrap_or(0);
            if (self.log_bytes > self.options.snapshot_bytes && self.log_bytes > size)
                || (many && self.log_bytes > size / 8)
            {
                self.snapshot_due = Some(Instant::now());
            }
        }
        let u = Upload {
            snapshot: None,
            number: self.batch,
            first_tx: first,
            last_tx: last,
            hash: self.prev_hash.clone(),
            statements,
            replies,
        };
        if let Some(up) = &self.uploader {
            if let Err(mpsc::SendError(u)) = up.tx.send(u) {
                for (_, reply) in u.replies {
                    let _ = reply.send(Err(CommitError::Failed("the uploader stopped".into())));
                }
            }
        }
    }

    /// One database transaction for these transactions, with their commit rows and the state.
    /// Returns every statement run (what the durability destinations receive).
    fn write(&mut self, items: &[(u64, Pending)]) -> Result<Vec<(String, Vec<Sql>)>, Failure> {
        let sqlite = self.conn.dialect() == Dialect::Sqlite;
        self.conn
            .batch(if sqlite { "BEGIN IMMEDIATE" } else { "BEGIN" })
            .map_err(Failure::Db)?;
        let prev = self.prev_hash.clone();
        let next_id = self.next_id;
        let durable = self.durable;
        let r = (|| -> Result<(Vec<(String, Vec<Sql>)>, String, u64), Failure> {
            if !sqlite {
                // a second writer (a process that should not be one) waits here instead of
                // interleaving
                self.conn
                    .batch("LOCK TABLE \"_quadra_commits\" IN EXCLUSIVE MODE")
                    .map_err(Failure::Db)?;
            }
            // nobody else may have committed since the last batch (a deploy, a migration, a
            // command line on the same database): then the numbers given are not free
            let head = crate::source::head(&mut self.conn).map_err(Failure::Db)?;
            if head != durable {
                return Err(Failure::Foreign);
            }
            let mut executed = Vec::new();
            let mut prev = prev.clone();
            let mut next_id = next_id;
            for (tx, p) in items {
                let (prepared, _, _) = p.submitted.as_ref().unwrap();
                for (sql, params) in &prepared.statements {
                    self.conn.exec(sql, params).map_err(Failure::Db)?;
                    executed.push((sql.clone(), params.clone()));
                }
                for d in &prepared.deltas {
                    executed.push(self.add(d).map_err(Failure::Db)?);
                }
                let mut h = CommitHasher::new(&prev, *tx, &p.at, p.user, &p.cause);
                for e in &prepared.payload {
                    h.entry(e);
                }
                let hash = h.finish();
                let row = (
                    "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text)".to_string(),
                    vec![Sql::Int(*tx as i64), Sql::Text(p.at.clone()), p.user.map(|u| Sql::Int(u as i64)).unwrap_or(Sql::Null), Sql::Text(p.cause.clone()), Sql::Text(prev.clone()), Sql::Text(hash.clone())],
                );
                self.conn.exec(&row.0, &row.1).map_err(Failure::Db)?;
                executed.push(row);
                prev = hash;
                next_id = next_id.max(prepared.next_id);
            }
            let last = items.last().unwrap().0;
            // the ids given to engines are past it too: after a restart none is given twice
            let next_id = next_id.max(self.ids_given);
            let state = (
                "INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('state', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"".to_string(),
                vec![Sql::Text(serde_json::json!({ "next_id": next_id, "tx": last }).to_string())],
            );
            self.conn.exec(&state.0, &state.1).map_err(Failure::Db)?;
            executed.push(state);
            let batch = (
                "INSERT INTO \"_quadra_batches\" (\"number\", \"first_tx\", \"last_tx\", \"hash\") VALUES (?bigint, ?bigint, ?bigint, ?text)".to_string(),
                vec![Sql::Int(self.batch as i64 + 1), Sql::Int(durable as i64 + 1), Sql::Int(last as i64), Sql::Text(prev.clone())],
            );
            self.conn.exec(&batch.0, &batch.1).map_err(Failure::Db)?;
            executed.push(batch);
            Ok((executed, prev, next_id))
        })();
        match r {
            Ok((executed, hash, next_id)) => {
                if let Err(e) = self.conn.batch("COMMIT") {
                    let _ = self.conn.batch("ROLLBACK");
                    return Err(Failure::Db(e));
                }
                self.prev_hash = hash;
                self.next_id = next_id;
                self.batch += 1;
                Ok(executed)
            }
            Err(e) => {
                let _ = self.conn.batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl State {
    /// Adds a delta to a column's current value; returns the statement that set it.
    fn add(&mut self, d: &DeltaOp) -> Result<(String, Vec<Sql>), String> {
        let read = format!(
            "SELECT CAST(\"{}\" AS TEXT) FROM \"{}\" WHERE \"id\" = ?bigint",
            d.column, d.table
        );
        let cur = self
            .conn
            .query(&read, &[Sql::Int(d.id as i64)])?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone());
        let value = if d.int {
            let a: i64 = cur.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
            let b: i64 = d
                .amount
                .parse()
                .map_err(|_| format!("not an integer: {}", d.amount))?;
            Sql::Int(a + b)
        } else {
            let a: rust_decimal::Decimal = cur
                .as_deref()
                .and_then(|v| v.parse().ok())
                .unwrap_or_default();
            let b: rust_decimal::Decimal = d
                .amount
                .parse()
                .map_err(|_| format!("not a number: {}", d.amount))?;
            Sql::Text((a + b).to_string())
        };
        let set = format!(
            "UPDATE \"{}\" SET \"{}\" = {} WHERE \"id\" = ?bigint",
            d.table, d.column, d.param
        );
        let params = vec![value, Sql::Int(d.id as i64)];
        self.conn.exec(&set, &params)?;
        Ok((set, params))
    }
}

/// An engine's way to commit through the sequencer: the transaction is numbered (or
/// refused), its writes prepared with that number, then handed over; the call returns once
/// they are durable.
pub struct SequencedSink<'m> {
    pub db: crate::Database,
    pub model: &'m quadra_ir::Model,
    pub sequencer: Sequencer,
    /// Some: the commit is handed over without waiting, its ticket kept here (the engine
    /// answers the client when the ticket completes); None: the call waits for durability
    pub tickets: Option<std::rc::Rc<std::cell::RefCell<Vec<std::sync::Arc<Ticket>>>>>,
}

/// Gives the reservation up if the writes never reach the sequencer (an error, a panic).
struct Reserved<'a> {
    sequencer: &'a Sequencer,
    tx: Option<u64>,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx {
            self.sequencer.cancel(tx);
        }
    }
}

thread_local! {
    /// (QUADRA_SEQ_STATS) commits, and time in: the write set, the reservation, preparing, the
    /// wait for durability
    static TIMES: std::cell::Cell<(u64, Duration, Duration, Duration, Duration)> = const { std::cell::Cell::new((0, Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO)) };
}

fn note_times(set: Duration, reserve: Duration, prepare: Duration, wait: Duration) {
    if std::env::var_os("QUADRA_SEQ_STATS").is_none() {
        return;
    }
    TIMES.with(|t| {
        let (n, a, b, c, d) = t.get();
        let (n, a, b, c, d) = (n + 1, a + set, b + reserve, c + prepare, d + wait);
        if n == 500 {
            let ms = |x: Duration| x.as_secs_f64() * 1000.0 / n as f64;
            eprintln!("engine {:?}: per commit write set {:.2} ms, reserve {:.2} ms, prepare {:.2} ms, durable {:.2} ms", std::thread::current().name(), ms(a), ms(b), ms(c), ms(d));
            t.set((0, Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO));
        } else {
            t.set((n, a, b, c, d));
        }
    });
}

impl quadra_runtime::CommitSink for SequencedSink<'_> {
    fn commit(&mut self, c: &quadra_runtime::Commit) -> Result<u64, CommitError> {
        let clock = Instant::now();
        let (writes, history, outbox) = self.db.write_set(c);
        let mut reads = c.after.reads.clone();
        let guards = guard_checks(c, &mut reads);
        let request = Request {
            base: c.base,
            whole: !c.after.lazy(),
            reads,
            writes,
            guards,
            history,
            outbox,
            at: c.time.format("%Y-%m-%dT%H:%M:%S").to_string(),
            user: c.user,
            cause: c.cause.clone(),
        };
        let t_set = clock.elapsed();
        let r = self.sequencer.reserve(request)?;
        let t_reserve = clock.elapsed();
        let mut guard = Reserved {
            sequencer: &self.sequencer,
            tx: Some(r.tx),
        };
        let prepared = self
            .db
            .prepare(self.model, c, &r)
            .map_err(CommitError::Failed)?;
        guard.tx = None;
        let t_prepare = clock.elapsed();
        if let Some(tickets) = &self.tickets {
            let ticket = std::sync::Arc::new(Ticket::default());
            let t = ticket.clone();
            self.sequencer
                .submit_then(r.tx, prepared, Box::new(move |res| t.complete(res)));
            tickets.borrow_mut().push(ticket);
            note_times(
                t_set,
                t_reserve - t_set,
                t_prepare - t_reserve,
                Duration::ZERO,
            );
            return Ok(r.tx);
        }
        let res = self.sequencer.submit(r.tx, prepared);
        note_times(
            t_set,
            t_reserve - t_set,
            t_prepare - t_reserve,
            clock.elapsed() - t_prepare,
        );
        // a limit met while the sequencer wrote (the database's size) is this job's answer
        if let Err(CommitError::Failed(e)) = &res {
            if e.starts_with("too large:") {
                quadra_runtime::limits::hit(e.lines().next().unwrap_or(e).to_string());
            }
        }
        res
    }
}

/// The guards of a transaction as the sequencer checks them; one it cannot check (not an
/// amount, another currency) becomes a read of the record's computed values.
fn guard_checks(c: &quadra_runtime::Commit, reads: &mut ReadSet) -> Vec<GuardCheck> {
    use quadra_runtime::store::{amount_of, delta_amount};
    let mut out = Vec::new();
    for g in &c.after.guards {
        let key = (g.id, g.field.clone());
        let check = c.after.deltas.get(&key).and_then(|(old, _)| {
            let now = c.after.materialized.get(&key)?;
            let own = delta_amount(old, now)?;
            let (seen, unit) = amount_of(old)?;
            let (bound, bound_unit) = amount_of(&g.bound)?;
            (unit == bound_unit || bound_unit.is_empty() && unit.is_empty()).then_some(GuardCheck {
                id: g.id,
                field: g.field.clone(),
                op: g.op,
                seen,
                own,
                bound,
            })
        });
        match check {
            Some(gc) => out.push(gc),
            None => reads.formula(g.id, &g.field),
        }
    }
    out
}

/// A batch in the database, to be kept at the destinations.
struct Upload {
    /// a snapshot of the database to keep instead of a batch: (batch, transaction, file)
    snapshot: Option<(u64, u64, std::path::PathBuf)>,
    number: u64,
    first_tx: u64,
    last_tx: u64,
    hash: String,
    statements: Vec<(String, Vec<Sql>)>,
    replies: Vec<(u64, Responder)>,
}

/// The thread that keeps batches at the durability destinations, in order. The batches that
/// wait while one is being kept go together, as one segment: a slow destination (object
/// storage) makes the segments larger, not the commits slower.
struct Uploader {
    tx: mpsc::Sender<Upload>,
    handle: Option<std::thread::JoinHandle<()>>,
    acked: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// set while it is told to stop: a batch it cannot keep is then given up
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Uploader {
    fn start(
        mut destinations: Vec<Box<dyn DurabilitySink>>,
        rule: Rule,
        acked: u64,
        health: std::sync::Arc<HealthCell>,
    ) -> Result<Uploader, String> {
        health
            .kept
            .store(acked, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<Upload>();
        let acked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(acked));
        let shared = acked.clone();
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = stopping.clone();
        let handle = std::thread::Builder::new()
            .name("quadra-uploader".into())
            .spawn(move || {
                // In order, and nothing goes out before what precedes it: the log never has a
                // gap. What is not kept is tried again; once that has failed a few times the
                // destinations count as down, and the clients waiting (and those arriving) hear
                // at once that their commit is in the database but not kept elsewhere, while
                // their batches wait in the queue. Told to stop, it gives up after ~10 s (the
                // next start sends a snapshot to a destination left behind).
                let mut queue: std::collections::VecDeque<Upload> = std::collections::VecDeque::new();
                let mut attempt = 0u32;
                let mut down: Option<String> = None;
                let mut open = true;
                let fail_waiting = |queue: &mut std::collections::VecDeque<Upload>, why: &str| {
                    for u in queue.iter_mut() {
                        for (_, reply) in u.replies.drain(..) {
                            let _ = reply.send(Err(CommitError::Failed(format!("the commit is in the database but not yet kept elsewhere: {why}"))));
                        }
                    }
                };
                loop {
                    if queue.is_empty() {
                        if !open {
                            break;
                        }
                        match rx.recv() {
                            Ok(u) => queue.push_back(u),
                            Err(_) => break,
                        }
                    }
                    loop {
                        match rx.try_recv() {
                            Ok(u) => queue.push_back(u),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                open = false;
                                break;
                            }
                        }
                    }
                    if let Some(why) = &down {
                        fail_waiting(&mut queue, why);
                    }
                    // the front: a snapshot, or the batches up to the next snapshot as one segment
                    let result = if let Some((batch, tx, file)) = queue.front().and_then(|u| u.snapshot.clone()) {
                        let r = destinations.iter_mut().try_for_each(|d| d.snapshot(batch, tx, &file));
                        if r.is_ok() {
                            let _ = std::fs::remove_file(&file);
                            queue.pop_front();
                            for d in destinations.iter_mut() {
                                if let Err(e) = d.prune(batch) {
                                    eprintln!("sequencer: what the snapshot at batch {batch} holds was not dropped: {e}");
                                }
                            }
                        }
                        r
                    } else {
                        let n = queue.iter().take_while(|u| u.snapshot.is_none()).count();
                        let group = queue.range(..n);
                        let statements: Vec<(String, Vec<Sql>)> = group.clone().flat_map(|u| u.statements.iter().cloned()).collect();
                        let (number, last_tx, hash) = queue.range(..n).last().map(|l| (l.number, l.last_tx, l.hash.clone())).unwrap();
                        let (first_number, first_tx) = (queue[0].number, queue[0].first_tx);
                        let batch = Batch { first_number, number, first_tx, last_tx, hash: &hash, statements: &statements };
                        // the clients hear once the rule is met; the batch leaves the queue only once
                        // every destination has it (under any rule: a gap would make the log useless)
                        let (all, met) = keep(&mut destinations, rule, &batch);
                        if met {
                            for u in queue.range_mut(..n) {
                                for (tx, reply) in u.replies.drain(..) {
                                    let _ = reply.send(Ok(tx));
                                }
                            }
                        }
                        if all.is_ok() {
                            shared.store(last_tx, std::sync::atomic::Ordering::SeqCst);
                            health.kept.store(last_tx, std::sync::atomic::Ordering::Relaxed);
                            queue.drain(..n);
                        }
                        all
                    };
                    match result {
                        Ok(()) => {
                            attempt = 0;
                            if down.take().is_some() {
                                *health.down.lock().unwrap() = None;
                                eprintln!("sequencer: the destinations are back");
                            }
                        }
                        Err(e) => {
                            attempt += 1;
                            if attempt >= 4 && down.is_none() {
                                eprintln!("sequencer: the destinations are down ({e}): commits are kept here until they are back");
                                fail_waiting(&mut queue, &e);
                                down = Some(e.clone());
                                *health.down.lock().unwrap() = Some(e.clone());
                            }
                            if attempt >= 6 && stopping.load(std::sync::atomic::Ordering::SeqCst) {
                                let last = queue.back().map_or(0, |u| u.last_tx);
                                eprintln!("sequencer: stopped with the commits up to {last} not kept elsewhere: {e}");
                                fail_waiting(&mut queue, &e);
                                for u in queue.drain(..) {
                                    if let Some((_, _, file)) = u.snapshot {
                                        let _ = std::fs::remove_file(file);
                                    }
                                }
                                continue;
                            }
                            // until the next try, the commits arriving while it is down hear it at once
                            let until = Instant::now() + Duration::from_millis((100u64 << attempt.min(6)).min(5000));
                            while open {
                                match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                                    Ok(u) => {
                                        queue.push_back(u);
                                        if let Some(why) = &down {
                                            fail_waiting(&mut queue, why);
                                        }
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                                    Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
                                }
                            }
                            if !open {
                                std::thread::sleep(until.saturating_duration_since(Instant::now()));
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Uploader {
            tx,
            handle: Some(handle),
            acked,
            stopping: stop,
        })
    }

    /// Keeps what it was given (a few tries for what does not go through), then stops.
    fn finish(mut self) {
        drop(self.tx);
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The destinations get the batch: whether all have it, and whether the rule of confirmation is
/// met (`Local`: always; `Any`: one has it; `All`: all have it).
fn keep(
    destinations: &mut [Box<dyn DurabilitySink>],
    rule: Rule,
    batch: &Batch,
) -> (Result<(), String>, bool) {
    let mut ok = 0;
    let mut errors = Vec::new();
    for d in destinations.iter_mut() {
        match d.persist(batch) {
            Ok(()) => ok += 1,
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        eprintln!(
            "sequencer: batch {}–{} not kept everywhere: {}",
            batch.first_tx,
            batch.last_tx,
            errors.join("; ")
        );
    }
    let met = match rule {
        Rule::Local => true,
        Rule::Any => ok > 0,
        Rule::All => errors.is_empty(),
    };
    let all = if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the commit is in the database but not yet kept elsewhere: {}",
            errors.join("; ")
        ))
    };
    (all, met)
}
