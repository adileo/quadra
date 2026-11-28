//! The engine: one thread per tenant. It owns the checked model, the record cache and a
//! database connection; the HTTP handlers send it jobs and await the reply. Commits are
//! serialized here, and by the database lock across processes.

use quadra_analysis::Checked;
use quadra_api::json::{cell, describe};
use quadra_ir::{EntityKind, Model};
use quadra_runtime::{Flow, Interp, Store, Units, V};
use quadra_sema::Project;
use quadra_store::auth::{self, Principal};
use quadra_store::conn::{Idle, Pooled};
use quadra_store::source::{head, load_renames, renamed, Cache, DbSource, Renames};
use quadra_store::{materialized_formulas, Conn, Database, Sink};
use serde_json::{json, Value as J};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use tokio::sync::oneshot;

mod apps;
mod complete;
mod consent;
mod dev;
mod ide;
mod interfaces;
mod local;
mod params;
mod schema;
mod sheets;
pub(crate) mod uploads;
mod views;
mod xlsx;

pub use apps::{app_params, bind_for, open_requests, unhex, AppRequest};
pub use local::Local;

#[derive(Clone)]
pub struct Config {
    pub root: PathBuf,
    pub url: String,
    pub port: u16,
    /// `POST /v1/auth/dev-login` with just a login: for local development only
    pub dev_login: bool,
    pub secure_cookies: bool,
    pub cache_mb: usize,
    /// load the records of the last N days into the cache at start
    pub prewarm_days: Option<i64>,
    /// the built web client, served at `/`
    pub web: Option<PathBuf>,
    /// seconds between sweeps of time-driven rules (0 = never)
    pub sweep_every: u64,
    /// logins allowed to edit the code from the browser (`*`: everyone)
    pub editors: Vec<String>,
    /// `quadra dev`: files changed on disk are checked and applied at once
    pub watch: bool,
    /// how long a job may run and how much it may read
    pub limits: Limits,
    /// how many engines serve the project at once
    pub engines: Engines,
    /// the values of quadra.toml's variables here, and whether events go out
    pub outside: Outside,
}

/// Text as it goes in a url: letters, digits and `-._~` as they are, the rest `%XX`.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What an inbound's lines say of a message (`ordered by`, `latest by … using`), read from
/// its record when it comes. A value that cannot be read leaves the message without it.
fn marks_of(
    model: &quadra_ir::Model,
    i: &quadra_ir::Inbound,
    doc: &quadra_runtime::V,
) -> quadra_store::deliveries::Marks {
    let eval = |e: &quadra_ir::Expr| -> Option<String> {
        let mut interp = quadra_runtime::Interp::new(model);
        let mut ctx = quadra_runtime::Ctx::default();
        ctx.bind(&i.binding, doc.clone());
        interp.eval(e, &mut ctx).ok().and_then(|v| match v {
            quadra_runtime::V::None => None,
            quadra_runtime::V::Text(t) => Some(t.to_string()),
            quadra_runtime::V::DateTime(t) => Some(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
            other => Some(other.to_string()),
        })
    };
    quadra_store::deliveries::Marks {
        lane: i.ordered.as_ref().and_then(eval),
        latest: i.latest.as_ref().and_then(|(k, _)| eval(k)),
        version: i.latest.as_ref().and_then(|(_, u)| eval(u)),
        ..Default::default()
    }
}

/// `n` random bytes, as hex.
fn agent_secret(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random generator");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The column of a table a pull's cursor reads (`cursor m.updated_at`: the field's `at`, else
/// its name).
fn cursor_column(
    model: &quadra_ir::Model,
    i: &quadra_ir::Inbound,
    e: &quadra_ir::Expr,
) -> Option<String> {
    let quadra_ir::ExprKind::Field { base, name, .. } = &e.kind else {
        return None;
    };
    if !matches!(&base.kind, quadra_ir::ExprKind::Name(b) if *b == i.binding) {
        return None;
    }
    let field = model
        .entity(&i.ty)
        .and_then(|r| model.find_field(r, name))?;
    Some(quadra_ir::formats::place(field).join("."))
}

/// The rows a pull of a table asks for at once; a pull that brings as many goes on at once.
const ROWS_PER_PULL: usize = 1000;

/// What is wrong with the rows a pull of a table brought for the inbound's record: a field
/// whose column (its `at`, else its name) is not in the table.
fn missing_column(
    model: &quadra_ir::Model,
    i: &quadra_ir::Inbound,
    table: &str,
    first: &[u8],
) -> Option<String> {
    let J::Object(cols) = serde_json::from_slice::<J>(first).ok()? else {
        return None;
    };
    let rec = model.entity(&i.ty)?;
    for f in quadra_ir::formats::fields(model, rec) {
        let col = quadra_ir::formats::place(f).join(".");
        if !cols.contains_key(&col) {
            return Some(format!(
                "the table {table} has no column `{col}` (the field `{}` of {}); its columns: {}",
                f.name,
                i.ty,
                cols.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    None
}

/// A checked project, its model given the values of the variables where it runs.
pub(crate) fn with_env(
    mut checked: Checked,
    env: &std::collections::BTreeMap<String, String>,
) -> Checked {
    if let Some(m) = checked.model.as_mut() {
        m.env = env.clone();
    }
    checked
}

/// Where the project meets the outside: the values of the variables of quadra.toml in this
/// place (`env.NAME`, the connections' credentials), and whether the events with an
/// `outbound` are delivered (`deliver`: `quadra serve` and `quadra dev`, a production
/// environment). Without it they stay in the outbox: tests, previews and forks, which must
/// never send with production's connections.
#[derive(Clone, Debug, Default)]
pub struct Outside {
    pub env: std::collections::BTreeMap<String, String>,
    pub deliver: bool,
}

/// The engines of a project. One engine serves one request at a time; more engines serve
/// more at once. Each has its own connection and its own cache of records (`cache_mb` each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engines {
    /// engines that only read: grids, records, searches, reports, the formulas of apps. They
    /// see every commit made before the request reached them.
    pub readers: usize,
    /// engines that prepare writes. The tenant's sequencer orders and writes the commits, so
    /// several may prepare at once on SQLite too; one is usually best (several bring conflicts
    /// on shared totals rather than speed).
    pub writers: usize,
}

impl Default for Engines {
    /// Two readers and a writer.
    fn default() -> Self {
        Engines {
            readers: 2,
            writers: 1,
        }
    }
}

impl Engines {
    /// A single engine: it reads and writes.
    pub fn one() -> Engines {
        Engines {
            readers: 0,
            writers: 1,
        }
    }
}

/// The limits of the engine's jobs (see `quadra_runtime::limits`): a request that loops or
/// reads too much stops with an error, and the next one is served as usual.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// a request: reads, writes, actions, the formulas of apps (`None`: no limit). On
    /// PostgreSQL it is also the time limit of each statement.
    pub job: Option<std::time::Duration>,
    /// sweeps of time-driven rules and the editor's long operations (tests, commits, schema
    /// changes)
    pub background: Option<std::time::Duration>,
    /// rows a single statement may return (`None`: no limit)
    pub max_rows: Option<usize>,
    /// elements a list or a set may have (`None`: no limit)
    pub max_list: Option<usize>,
    /// requests waiting for the engine: past it a request is refused at once (503 with
    /// `Retry-After`) rather than queued behind a backlog it would time out in (`None`: no
    /// limit)
    pub max_queue: Option<usize>,
    /// SQLite: the size the database file may grow to; past it a write fails and the
    /// database stays as it was (`None`: no limit). PostgreSQL: not enforced here.
    pub max_bytes: Option<u64>,
    /// users (`User` records): creating one more fails (`None`: no limit)
    pub max_users: Option<u64>,
}

impl Limits {
    /// No limits: the engine runs in the process of a command of one's own.
    pub fn none() -> Limits {
        Limits {
            job: None,
            background: None,
            max_rows: None,
            max_list: None,
            max_queue: None,
            max_bytes: None,
            max_users: None,
        }
    }
}

impl Default for Limits {
    /// For a server of one's own: two minutes per request, an hour for the long operations,
    /// no caps on rows and lists, at most 1000 requests waiting, no limit on the size.
    fn default() -> Self {
        Limits {
            job: Some(std::time::Duration::from_secs(120)),
            background: Some(std::time::Duration::from_secs(3600)),
            max_rows: None,
            max_list: None,
            max_queue: Some(1000),
            max_bytes: None,
            max_users: None,
        }
    }
}

pub struct Reply {
    pub status: u16,
    pub body: J,
    /// a new session secret to set as cookie
    pub session: Option<String>,
}

/// The answer to a request whose commit failed after it was handed to the sequencer.
fn committed_late(e: quadra_runtime::CommitError) -> Reply {
    match e {
        quadra_runtime::CommitError::Conflict(c) => {
            let list: Vec<J> = c.iter().map(|x| json!({ "tx": x.tx, "at": x.at, "user": x.user, "cause": x.cause, "what": x.what })).collect();
            reply(
                409,
                json!({ "error": "changed by others while it was being saved: try again", "conflicts": list }),
            )
        }
        quadra_runtime::CommitError::Failed(m) if m.starts_with("too large:") => {
            error(422, m.lines().next().unwrap_or(&m).to_string())
        }
        quadra_runtime::CommitError::Failed(m) => error(500, format!("database: {m}")),
    }
}

fn reply(status: u16, body: J) -> Reply {
    Reply {
        status,
        body,
        session: None,
    }
}

/// How long a session made by the sign-in by email lasts.
pub const SESSION_DAYS: i64 = 7;

fn error(status: u16, msg: impl Into<String>) -> Reply {
    reply(status, json!({ "error": msg.into() }))
}

pub enum Req {
    Me,
    Model,
    Query(J),
    Record {
        entity: String,
        id: u64,
        base: Option<u64>,
    },
    History {
        entity: String,
        id: u64,
        limit: Option<usize>,
        before: Option<u64>,
    },
    /// what a record holds about its person, exported (a commit records who and when)
    Export {
        entity: String,
        id: u64,
    },
    /// the records in the trash the caller may delete (of `entity`, or of every entity)
    Trash {
        entity: Option<String>,
    },
    /// the file a `File` field names (the `n`th of a list): a signed URL of it
    File {
        entity: String,
        id: u64,
        field: String,
        n: Option<usize>,
        download: bool,
    },
    /// `POST /v1/uploads`: a signed URL to upload a file of a `File` field to
    Upload {
        body: J,
    },
    /// the uploads a transaction names whose bytes are still to be checked (engine/uploads.rs)
    Uploads {
        body: J,
        key: String,
    },
    /// the files of uploads checked, kept before the transaction that names them
    UploadsChecked {
        body: J,
    },
    Search {
        entity: String,
        q: String,
        limit: usize,
        after: Option<String>,
    },
    Tx {
        body: J,
        key: String,
    },
    DevFiles,
    DevRead {
        path: String,
    },
    DevSave {
        path: String,
        text: String,
    },
    DevDelete {
        path: String,
    },
    DevCheck {
        buffers: J,
    },
    DevTest {
        buffers: J,
    },
    DevStatus,
    DevDiff,
    DevCommit {
        message: String,
    },
    DevIde {
        body: J,
    },
    DevRename {
        from: String,
        to: String,
    },
    DevMkdir {
        path: String,
    },
    DevRmdir {
        path: String,
    },
    DevSchema {
        body: J,
    },
    DevLive,
    /// `/v1/views…`, `/v1/teams…`, `/v1/directory` (engine/views.rs)
    Views {
        method: String,
        path: String,
        query: HashMap<String, String>,
        body: J,
    },
    /// `/v1/outbox`: an integration takes the events no `outbound` delivers, and says what
    /// came of them (`crate::deliver`)
    Outbox {
        path: String,
        query: HashMap<String, String>,
        body: J,
    },
    /// a request that fails with an internal error (tests of the recovery)
    #[cfg(test)]
    Panic,
}

impl Job {
    /// Only reads the database: any engine of the tenant can do it. (A misrouted write would
    /// still be correct, the database serializing commits, only slower.)
    pub(crate) fn reads(&self) -> bool {
        match self {
            Job::Auth { .. } | Job::Commits { .. } => true,
            Job::Call { req, .. } => req.reads(),
            _ => false,
        }
    }
}

impl Job {
    /// A write of the project's data: any engine may run it, the sequencer orders the commits.
    pub(crate) fn writes_data(&self) -> bool {
        matches!(
            self,
            Job::Call {
                req: Req::Tx { .. } | Req::Views { .. },
                ..
            }
        )
    }
}

impl Req {
    fn reads(&self) -> bool {
        match self {
            Req::Me
            | Req::Model
            | Req::Query(_)
            | Req::Record { .. }
            | Req::History { .. }
            | Req::Trash { .. }
            | Req::File { .. }
            | Req::Search { .. } => true,
            // formulas evaluated as the caller and completions are reads sent as `POST`
            Req::Views { method, path, .. } => {
                method == "GET" || path == "/v1/eval" || path == "/v1/complete"
            }
            // the editor keeps its state on the first engine
            _ => false,
        }
    }

    /// The editor's operations that may take long: they get the background limit.
    pub(crate) fn is_long(&self) -> bool {
        matches!(
            self,
            Req::DevTest { .. } | Req::DevCommit { .. } | Req::DevSchema { .. }
        )
    }
}

pub enum Job {
    Auth {
        secret: String,
        reply: oneshot::Sender<Option<Principal>>,
    },
    DevLogin {
        login: String,
        reply: oneshot::Sender<Reply>,
    },
    /// sign-in by email: a code for the user of `login` (or of that email)
    CodeStart {
        login: String,
        reply: oneshot::Sender<Reply>,
    },
    CodeVerify {
        id: String,
        code: String,
        reply: oneshot::Sender<Reply>,
    },
    /// an admin invites a user: where to write
    Invite {
        who: Principal,
        user: u64,
        reply: oneshot::Sender<Reply>,
    },
    /// the host (a console): the roles of the project and its users
    /// when the next sweep is needed (`Host::next_sweep`): `Err(())` periodic, `Ok(None)`
    /// never, `Ok(Some(at))` at that instant
    NextSweep {
        reply: oneshot::Sender<Result<Option<chrono::NaiveDateTime>, ()>>,
    },
    /// the store of the project's files (`[files]` of quadra.toml)
    Files {
        reply: oneshot::Sender<Result<std::sync::Arc<quadra_blobs::Files>, String>>,
    },
    /// the documents of `attach` due, rendered and named in their records (up to `limit`):
    /// how many, and whether the model has any `attach`
    Attach {
        limit: usize,
        reply: oneshot::Sender<Result<(usize, bool), String>>,
    },
    /// the host's deliveries (`crate::deliver`): the plans of up to `limit` due events of the
    /// outbox, when the next pending one is due after them (`None`: none pending), and whether
    /// the model has any `outbound` at all
    Deliveries {
        limit: usize,
        reply: oneshot::Sender<
            Result<
                (
                    Vec<crate::deliver::Plan>,
                    Option<chrono::NaiveDateTime>,
                    bool,
                ),
                String,
            >,
        >,
    },
    /// a message sent to `/in/<connection><path>`: checked and kept in the inbox
    /// (`crate::inbound`); the answer for the sender
    Receive {
        connection: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        reply: oneshot::Sender<(u16, J)>,
    },
    /// a request to an `endpoint` (`/in/<connection><path>`), answered at once
    Endpoint {
        connection: String,
        method: String,
        path: String,
        query: HashMap<String, String>,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        reply: oneshot::Sender<Option<(u16, J)>>,
    },
    /// the pulls due now (`inbound … pull`), and when the next one is due after them
    Pulls {
        reply: oneshot::Sender<(Vec<crate::deliver::Pull>, Option<chrono::NaiveDateTime>)>,
    },
    /// an agent registers with its one-time code: its name and the key it signs in with
    AgentHello {
        code: String,
        version: String,
        reply: oneshot::Sender<Result<(String, String), String>>,
    },
    /// the agent whose key this is, if it is registered and not revoked
    AgentKey {
        key: String,
        version: String,
        reply: oneshot::Sender<Option<String>>,
    },
    /// what a pull brought: its messages kept in the inbox, its cursor moved; how many new
    Pulled {
        inbound: String,
        result: crate::deliver::PullResult,
        reply: oneshot::Sender<usize>,
    },
    /// the host's inbox: up to `limit` messages received handled; how many, and whether the
    /// model has any `inbound`
    Inbox {
        limit: usize,
        reply: oneshot::Sender<(usize, bool)>,
    },
    /// event `id` not tried before `at` (its connection is paused), without an attempt
    Defer {
        id: u64,
        at: chrono::NaiveDateTime,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// what came of the delivery of event `id`, written through the sequencer
    Delivered {
        id: u64,
        event: String,
        outcome: quadra_store::deliveries::Outcome,
        attempt: quadra_store::deliveries::Attempt,
        /// its fields, the attempts made before this one and the answer: for the reactions to
        /// its outcome (`d.reply`)
        fields: J,
        attempts: u64,
        answer: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    People {
        reply: oneshot::Sender<Reply>,
    },
    /// the host adds a user (as the system: no one's permissions apply) and invites them
    AddUser {
        email: String,
        roles: Vec<String>,
        reply: oneshot::Sender<Reply>,
    },
    Logout {
        secret: String,
    },
    Call {
        who: Principal,
        req: Req,
        reply: oneshot::Sender<Reply>,
    },
    Commits {
        since: u64,
        reply: oneshot::Sender<(u64, Vec<J>)>,
    },
    /// a pass of the rules that read the clock; the answer (if asked): the transaction written
    Sweep {
        reply: Option<oneshot::Sender<Result<Option<u64>, String>>>,
    },
    /// `quadra dev`: the files changed; the event to broadcast, if any
    DevReload {
        reply: oneshot::Sender<Option<J>>,
    },
    /// `quadra dev`: the data changed while the saved code did not fit it: try it again
    DevRetry {
        reply: oneshot::Sender<Option<J>>,
    },
    /// OAuth2 with a user's consent: a step of the provider's callback (`crate::oauth`)
    Consent {
        step: crate::oauth::Step,
        reply: oneshot::Sender<Result<crate::oauth::Next, String>>,
    },
    /// the host stops the tenant: the database is closed when the answer comes
    Stop {
        reply: oneshot::Sender<()>,
    },
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".into())
}

/// The stack of an engine's thread. A user's function may call itself up to the
/// interpreter's limit (200 calls deep), and every call is several frames of the
/// interpreter: a stack overflow cannot be caught, and would end the whole process. Only
/// the pages actually used take memory.
pub const ENGINE_STACK: usize = 256 << 20;

pub fn now() -> chrono::NaiveDateTime {
    // an instant, in UTC (docs: a DateTime is an instant; the project's zone reads it)
    let t = chrono::Utc::now().naive_utc();
    t - chrono::Duration::nanoseconds(t.and_utc().timestamp_subsec_nanos() as i64)
}

pub struct Engine {
    root: PathBuf,
    files: Vec<(PathBuf, String)>,
    checked: Rc<Checked>,
    /// the values of quadra.toml's variables here (`Outside::env`): every model it checks gets them
    env: std::collections::BTreeMap<String, String>,
    url: String,
    schema: Rc<quadra_store::schema::Schema>,
    cache: Rc<RefCell<Cache>>,
    conn: Conn,
    /// idle connections reused by the requests
    pool: Idle,
    /// the renames of fields (they change only with a migration)
    renames: Option<Renames>,
    /// units, currencies and the company, as of a transaction: read again only when one of
    /// those records is written
    units: Option<(Units, Option<u64>, u64)>,
    overlays: HashMap<String, Rc<Checked>>,
    overlay_order: VecDeque<String>,
    dev_login: bool,
    /// source file → module name (`sales`, `core/base`; `std` for the standard library)
    modules: HashMap<quadra_sema::FileId, String>,
    editors: Vec<String>,
    /// the analysed working copy for editor features, and what it was computed from
    ide_cache: Option<(String, Rc<ide::IdeState>)>,
    /// the last analysis with a model: types while the code is being typed
    ide_types: Option<Rc<ide::IdeState>>,
    /// bumped when files of the working copy change
    dev_generation: u64,
    /// `quadra dev`: watching the files, the files last looked at, what is wrong with them
    watch: bool,
    seen: Vec<(PathBuf, String)>,
    problems: Option<J>,
    /// checked workbooks and the values of formulas of apps (engine/sheets.rs)
    sheets: sheets::SheetState,
    /// checked custom forms (engine/interfaces.rs), by their generated source
    form_checks: HashMap<String, (Option<Rc<Checked>>, Vec<String>)>,
    /// `quadra dev`: the interface files last looked at (`None`: look again)
    seen_interfaces: Option<Vec<(PathBuf, String)>>,
    limits: Limits,
    cache_mb: usize,
    /// the request being served only reads: its interpreters read one moment of the database
    snapshot: bool,
    /// the tenant's sequencer: commits go through it (none: written directly, as the
    /// command line does)
    sequencer: Option<quadra_store::Sequencer>,
    /// when the history was last archived (`[data] archive_after`)
    archived_at: Option<std::time::Instant>,
    /// when the closed records last went cold (`archive …`)
    cold_at: Option<std::time::Instant>,
    /// when the log of the integrations was last pruned (`[integrations] log_days`)
    pruned_at: Option<std::time::Instant>,
    /// the documents of an `attach` that could not be rendered (document, record): not tried
    /// again until the code changes
    attach_failed: (u64, std::collections::HashSet<(String, u64)>),
    /// when the uploads of more than a day were last removed (engine/uploads.rs)
    uploads_pruned_at: Option<std::time::Instant>,
    /// when each agent's visit was last written
    agents_seen: HashMap<String, std::time::Instant>,
    /// the causes of the last commits before this engine handled its first message: only a
    /// message among them can have been handled without being marked (the server stopped in
    /// between), so only those are looked for in the whole log
    inbox_recent: Option<Vec<String>>,
    /// the analysis of each model in use (the project's, an app's), shared by the requests: kept
    /// with the model it is of, so that it never outlives it
    statics: Vec<(Rc<Checked>, Rc<quadra_runtime::Statics>)>,
    /// commits of the request being served handed to the sequencer without waiting: its client
    /// is answered once they are durable, while the engine serves the next request
    tickets: Rc<RefCell<Vec<std::sync::Arc<quadra_store::sequencer::Ticket>>>>,
    /// wrote outside the hash chain before the sequencer was there (interface files, views
    /// checked at the start): the log gets it with a snapshot once it is (`set_sequencer`)
    unlogged: bool,
    /// the interface files installed changed something
    pub(crate) installed_any: bool,
}

type Fail = (u16, String);

/// The interface files of a project, as `quadra dev` compares them.
fn interface_texts(project: &Project) -> Vec<(PathBuf, String)> {
    project
        .interface_files()
        .map(|f| (f.path.clone(), f.text.clone()))
        .collect()
}

impl Engine {
    /// The project's first engine: it brings the database up to date with the project
    /// (indexes, the apps of interface files, saved views checked again), then serves.
    pub fn new(cfg: &Config) -> Result<Engine, String> {
        Engine::open(cfg, true)
    }

    /// One more engine of a project whose first engine is running: it serves at once.
    pub fn follower(cfg: &Config) -> Result<Engine, String> {
        Engine::open(cfg, false)
    }

    fn open(cfg: &Config, first: bool) -> Result<Engine, String> {
        let project =
            Project::load(&cfg.root).map_err(|e| format!("cannot read the project: {e}"))?;
        let files: Vec<(PathBuf, String)> = project
            .user_files()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        let modules = project
            .files()
            .map(|f| {
                (
                    f.id,
                    if f.kind == quadra_sema::FileKind::Std {
                        "std".to_string()
                    } else {
                        f.module.clone()
                    },
                )
            })
            .collect();
        let checked = with_env(quadra_analysis::check(&project), &cfg.outside.env);
        let Some(model) = &checked.model else {
            return Err("the project has errors: run `quadra check`".into());
        };
        // where events go out, every variable has its value: a missing credential would only
        // show at the first delivery
        if cfg.outside.deliver {
            model.manifest.check_values(&cfg.outside.env)?;
        }
        // the interface files are part of a version: one that does not check is not served
        let interface_errors = checked
            .diagnostics
            .iter()
            .filter(|d| {
                d.severity == quadra_sema::Severity::Error
                    && project.file(d.file).kind == quadra_sema::FileKind::Interface
            })
            .count();
        if interface_errors > 0 {
            return Err(format!(
                "the interface files have {interface_errors} error(s): run `quadra check`"
            ));
        }
        let mut db = Database::open(&cfg.url, model, &checked.report)?;
        let status = db.status()?;
        if !status.initialized {
            return Err("the database is not initialized: run `quadra db init`".into());
        }
        if !status.schema_current {
            return Err("the schema changed: run `quadra db migrate`".into());
        }
        let schema = db.schema.clone();
        // indexes a crash or an older version left missing (PostgreSQL: built concurrently)
        if first {
            if let Err(e) = db.ensure_indexes(None) {
                eprintln!("indexes: {e}");
            }
        }
        drop(db);
        // PostgreSQL: the connections of this engine stop a statement at the request's limit
        quadra_store::conn::set_statement_timeout(cfg.limits.job);
        quadra_store::conn::set_max_bytes(cfg.limits.max_bytes);
        let mut conn = Conn::open(&cfg.url)?;
        if first && conn.dialect() == quadra_store::conn::Dialect::Sqlite {
            // statistics for SQLite's planner where it has none (a database grown by commits):
            // row rules in SQL start from the small tables. A sample of each index: fast.
            if let Err(e) = conn.batch("PRAGMA analysis_limit = 1000; PRAGMA optimize = 0x10002") {
                eprintln!("statistics: {e}");
            }
        }
        let cache = Rc::new(RefCell::new(Cache::new(
            cfg.cache_mb << 20,
            std::time::Duration::from_secs(600),
        )));
        if let Some(days) = cfg.prewarm_days {
            let n = cache
                .borrow_mut()
                .prewarm(&mut conn, now() - chrono::Duration::days(days))?;
            eprintln!("cache: {n} recent records loaded");
        }
        let seen = files.clone();
        let mut engine = Engine {
            env: cfg.outside.env.clone(),
            root: cfg.root.clone(),
            files,
            checked: Rc::new(checked),
            url: cfg.url.clone(),
            schema,
            cache,
            conn,
            pool: Rc::new(RefCell::new(Vec::new())),
            renames: None,
            units: None,
            overlays: HashMap::new(),
            overlay_order: VecDeque::new(),
            dev_login: cfg.dev_login,
            modules,
            editors: cfg.editors.clone(),
            ide_cache: None,
            ide_types: None,
            dev_generation: 0,
            watch: cfg.watch,
            seen,
            problems: None,
            sheets: Default::default(),
            form_checks: HashMap::new(),
            seen_interfaces: None,
            limits: cfg.limits,
            cache_mb: cfg.cache_mb,
            snapshot: false,
            sequencer: None,
            pruned_at: None,
            archived_at: None,
            cold_at: None,
            attach_failed: (0, Default::default()),
            uploads_pruned_at: None,
            agents_seen: HashMap::new(),
            inbox_recent: None,
            statics: Vec::new(),
            tickets: Rc::new(RefCell::new(Vec::new())),
            unlogged: false,
            installed_any: false,
        };
        if first {
            // the volumes of the cold records, beside the database before anything reads
            if let Err(e) = engine.fetch_cold() {
                eprintln!("cold: {e}");
            }
            // the apps and views of the interface files, as the files say
            engine.install_interfaces(&project)?;
            // saved views checked against this model (renames of the migrations applied)
            let changed = engine.revalidate_views();
            // written here before the sequencer is there: the log gets them with a snapshot
            engine.unlogged = changed > 0 || engine.installed_any;
        }
        engine.seen_interfaces = Some(interface_texts(&project));
        Ok(engine)
    }

    /// The shared analysis of `model` (one of the models this engine holds).
    fn statics_of(&mut self, model: &quadra_ir::Model) -> Rc<quadra_runtime::Statics> {
        let is = |c: &Rc<Checked>| c.model.as_ref().is_some_and(|m| std::ptr::eq(m, model));
        if let Some((_, s)) = self.statics.iter().find(|(c, _)| is(c)) {
            return s.clone();
        }
        let owner = std::iter::once(&self.checked)
            .chain(self.overlays.values())
            .find(|c| is(c))
            .cloned();
        let s = Rc::new(quadra_runtime::Statics::default());
        if let Some(c) = owner {
            if self.statics.len() >= 8 {
                self.statics.remove(0);
            }
            self.statics.push((c, s.clone()));
        }
        s
    }

    pub fn set_sequencer(&mut self, s: Option<quadra_store::Sequencer>) {
        if let (Some(s), true) = (&s, self.unlogged) {
            s.snapshot_soon(std::time::Duration::ZERO);
            self.unlogged = false;
        }
        self.sequencer = s;
    }

    /// A change of the tables outside the hash chain (sessions, tokens, sign-in codes, saved
    /// views, apps, folders, teams): through the tenant's sequencer when there is one, so that it
    /// is in the log with the data and survives the machine; on this connection otherwise.
    pub(crate) fn side<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut quadra_store::Conn) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        match &self.sequencer {
            Some(s) => s.side(f),
            None => f(&mut self.conn),
        }
    }

    /// The documents of the `attach`es due: rendered from the data as it is, their bytes kept
    /// in the store of files and what they are in `_quadra_files` (through the sequencer: in
    /// the log), then named in their records, each in a transaction of its own. A document that
    /// cannot be rendered is said once and not tried again until the code changes. The uploads
    /// of more than a day go on the way.
    fn attach(&mut self, limit: usize) -> Result<(usize, bool), String> {
        self.prune_uploads();
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return Ok((0, false));
        };
        if model.attaches.is_empty() {
            return Ok((0, false));
        }
        let code = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for (p, t) in &model.sources {
                p.hash(&mut h);
                t.hash(&mut h);
            }
            h.finish()
        };
        if self.attach_failed.0 != code {
            self.attach_failed = (code, Default::default());
        }
        let (done, failed) = quadra_store::ops::attach_render(
            &self.url,
            model,
            &checked.report,
            now(),
            limit,
            &self.attach_failed.1,
        )?;
        for (doc, id, e) in failed {
            eprintln!("attach {doc}: record {id}: {e}");
            self.attach_failed.1.insert((doc, id));
        }
        if !done.is_empty() {
            let files = self.files()?;
            for d in &done {
                files.keep(&d.file.hash, &d.bytes, &d.file.ctype, &d.file.name)?;
                let file = d.file.clone();
                self.side(move |c| quadra_store::files::put(c, &file))?;
            }
        }
        let n = quadra_store::ops::attach_write(
            &self.url,
            model,
            &checked.report,
            now(),
            self.sequencer.as_ref(),
            &done,
        )?;
        Ok((n, true))
    }

    /// The plans of up to `limit` due events with an `outbound`, and when the next pending one
    /// is due (`crate::deliver`).
    fn deliveries(
        &mut self,
        limit: usize,
    ) -> Result<
        (
            Vec<crate::deliver::Plan>,
            Option<chrono::NaiveDateTime>,
            bool,
        ),
        String,
    > {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return Ok((Vec::new(), None, false));
        };
        // once an hour: the log older than `[integrations] log_days` goes
        if self
            .pruned_at
            .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(3600))
            && limit > 0
        {
            self.pruned_at = Some(std::time::Instant::now());
            let before = (now() - chrono::Duration::days(model.manifest.log_days() as i64))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            if let Err(e) = self.side(move |c| quadra_store::deliveries::prune_log(c, &before)) {
                eprintln!("integrations: the old log was not pruned: {e}");
            }
            self.prune_messages(model);
        }
        if model.outbounds.is_empty() {
            return Ok((Vec::new(), None, false));
        }
        let events: Vec<String> = model.outbounds.iter().map(|o| o.event.clone()).collect();
        let at = now();
        // only what its sequencer has confirmed (without one, every commit on this connection is)
        let durable = match &self.sequencer {
            Some(s) => s.durable()?,
            None => u64::MAX,
        };
        let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let now_s = stamp(at);
        // the keys of `ordered by` and `latest by`, computed once per event, as of its emit
        for o in model
            .outbounds
            .iter()
            .filter(|o| o.policy.ordered.is_some() || o.policy.latest.is_some())
        {
            let todo =
                quadra_store::deliveries::without_keys(&mut self.conn, &o.event, durable, 200)?;
            for d in todo {
                let key = |e: &Option<quadra_ir::Expr>| -> String {
                    e.as_ref()
                        .map(|e| {
                            quadra_store::ops::eval_for_event(
                                &self.url,
                                model,
                                &checked.report,
                                at,
                                o,
                                e,
                                &d.fields,
                                d.tx,
                                true,
                            )
                            .map(|v| v.to_string())
                            .unwrap_or_else(|e| format!("error: {e}"))
                        })
                        .unwrap_or_default()
                };
                let (lane, latest) = (key(&o.policy.ordered), key(&o.policy.latest));
                let (name, stamp_now) = (o.event.clone(), now_s.clone());
                self.side(move |c| {
                    quadra_store::deliveries::set_keys(c, d.id, &name, &stamp_now, &lane, &latest)
                })?;
            }
        }
        // the events sent one by one, then those of each batch (up to its size)
        let single: Vec<String> = model
            .outbounds
            .iter()
            .filter(|o| o.batch.is_none())
            .map(|o| o.event.clone())
            .collect();
        let mut due =
            quadra_store::deliveries::due(&mut self.conn, &single, &now_s, durable, limit)?;
        for o in model.outbounds.iter().filter(|o| o.batch.is_some()) {
            let n = o.batch.as_ref().map_or(1, |b| b.0.max(1)) as usize;
            due.extend(quadra_store::deliveries::due(
                &mut self.conn,
                std::slice::from_ref(&o.event),
                &now_s,
                durable,
                n,
            )?);
        }
        let mut plans: Vec<crate::deliver::Plan> = Vec::new();
        let mut groups: Vec<(&quadra_ir::Outbound, Vec<quadra_store::deliveries::Due>)> =
            Vec::new();
        for d in due {
            let Some(o) = model.outbounds.iter().find(|o| o.event == d.event) else {
                continue;
            };
            let p = &o.policy;
            let seconds = |e: &Option<quadra_ir::Expr>| -> Option<u64> {
                let v = quadra_store::ops::eval_fixed(model, e.as_ref()?).ok()?;
                match v {
                    quadra_runtime::V::Duration(s) => Some(s.max(0) as u64),
                    _ => None,
                }
            };
            let created = chrono::NaiveDateTime::parse_from_str(&d.created_at, "%Y-%m-%dT%H:%M:%S")
                .unwrap_or(at);
            // `expires after`: no longer worth sending
            if let Some(s) = seconds(&p.expires) {
                if at > created + chrono::Duration::seconds(s as i64) {
                    let (name, t) = (o.event.clone(), now_s.clone());
                    let why = format!(
                        "expired: not sent within {} of its emit",
                        crate::deliver::human(s)
                    );
                    let reason = why.clone();
                    self.side(move |c| {
                        quadra_store::deliveries::finish(c, d.id, &name, &t, "expired", &why)
                    })?;
                    self.not_sent(
                        d.id,
                        &o.event,
                        quadra_ir::Trigger::Expired,
                        &d.fields,
                        d.attempts,
                        &reason,
                    );
                    continue;
                }
            }
            // `delay`, `window`: not yet
            let mut not_before =
                seconds(&p.delay).map(|s| created + chrono::Duration::seconds(s as i64));
            if let Some(w) = &p.window {
                if let Some(open) = crate::deliver::window_opens(&model.timezone, w, at) {
                    not_before = Some(not_before.map_or(open, |n| n.max(open)));
                }
            }
            if let Some(nb) = not_before.filter(|nb| *nb > at) {
                let t = stamp(nb);
                self.side(move |c| quadra_store::deliveries::defer(c, d.id, &t))?;
                continue;
            }
            // `unless`: on the data as it is now
            if let Some(u) = &p.unless {
                let r = quadra_store::ops::eval_for_event(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    o,
                    u,
                    &d.fields,
                    d.tx,
                    false,
                );
                if matches!(r, Ok(quadra_runtime::V::Bool(true))) {
                    let (name, t) = (o.event.clone(), now_s.clone());
                    let why = format!("unless {}", quadra_ir::print::expr(u));
                    let reason = why.clone();
                    self.side(move |c| {
                        quadra_store::deliveries::finish(c, d.id, &name, &t, "skipped", &why)
                    })?;
                    self.not_sent(
                        d.id,
                        &o.event,
                        quadra_ir::Trigger::Skipped,
                        &d.fields,
                        d.attempts,
                        &reason,
                    );
                    continue;
                }
            }
            // a batch: gathered, sent below
            if o.batch.is_some() {
                match groups.iter_mut().find(|(g, _)| g.event == o.event) {
                    Some((_, list)) => list.push(d),
                    None => groups.push((o, vec![d])),
                }
                continue;
            }
            let key = p.key.as_ref().map(|k| {
                quadra_store::ops::eval_for_event(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    o,
                    k,
                    &d.fields,
                    d.tx,
                    true,
                )
                .map(|v| match v {
                    quadra_runtime::V::Text(t) => t.to_string(),
                    other => other.to_string(),
                })
            });
            // the request its block builds, reading the data as of the emitting commit
            let built = o.body.as_ref().map(|_| {
                quadra_store::ops::build_request(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    o,
                    &d.fields,
                    d.tx,
                )
                .and_then(|v| crate::deliver::built_of(&v))
            });
            let mut plan = crate::deliver::plan(
                &model.manifest,
                &self.env,
                d,
                &o.connection,
                o.path.as_deref(),
                built,
            );
            let mut retry = crate::deliver::Retry::default();
            if let Some(s) = seconds(&p.retry_for) {
                retry.for_s = s;
            }
            if let Some(quadra_ir::Expr {
                kind: quadra_ir::ExprKind::Lit(quadra_ir::Lit::Number(n)),
                ..
            }) = &p.retry_times
            {
                retry.times = n.parse().ok();
                // the attempts decide, not the time (ten years)
                retry.for_s = 10 * 365 * 86_400;
            }
            if let Some(quadra_ir::Expr {
                kind: quadra_ir::ExprKind::Binary { lhs, rhs, .. },
                ..
            }) = &p.backoff
            {
                let one = |e: &quadra_ir::Expr| match quadra_store::ops::eval_fixed(model, e) {
                    Ok(quadra_runtime::V::Duration(s)) => Some(s.max(1) as u64),
                    _ => None,
                };
                if let (Some(a), Some(b)) = (one(lhs), one(rhs)) {
                    retry.first_s = a;
                    retry.max_s = b;
                }
            }
            plan.retry = retry;
            match key {
                Some(Ok(k)) => plan.set_key(&k),
                Some(Err(e)) => {
                    plan.send =
                        crate::deliver::Send::Broken(format!("the key of the outbound failed: {e}"))
                }
                None => {}
            }
            plans.push(plan);
        }
        // the batches full, or whose oldest event waited `within`; the others wait for that
        let mut batch_next: Option<chrono::NaiveDateTime> = None;
        for (o, list) in groups {
            let (n, within) = o.batch.clone().unwrap_or((1, None));
            let within_s = within
                .as_ref()
                .and_then(|w| match quadra_store::ops::eval_fixed(model, w) {
                    Ok(quadra_runtime::V::Duration(s)) => Some(s.max(0)),
                    _ => None,
                })
                .unwrap_or(0);
            let oldest = list
                .iter()
                .filter_map(|d| {
                    chrono::NaiveDateTime::parse_from_str(&d.created_at, "%Y-%m-%dT%H:%M:%S").ok()
                })
                .min()
                .unwrap_or(at);
            let ready_at = oldest + chrono::Duration::seconds(within_s);
            if list.len() < n as usize && ready_at > at {
                batch_next = Some(batch_next.map_or(ready_at, |b| b.min(ready_at)));
                continue;
            }
            for chunk in list.chunks(n.max(1) as usize) {
                let events: Vec<(J, u64)> =
                    chunk.iter().map(|d| (d.fields.clone(), d.tx)).collect();
                let built = quadra_store::ops::build_batch_request(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    o,
                    &events,
                )
                .and_then(|v| crate::deliver::built_of(&v));
                let first = chunk[0].clone();
                let key = format!(
                    "quadra-batch-{}-{}-{}",
                    first.commit.get(..16).unwrap_or(&first.commit),
                    first.id,
                    chunk.last().map_or(first.id, |d| d.id)
                );
                let mut plan = crate::deliver::plan(
                    &model.manifest,
                    &self.env,
                    first,
                    &o.connection,
                    o.path.as_deref(),
                    Some(built),
                );
                plan.set_key(&key);
                plan.batch = chunk[1..]
                    .iter()
                    .map(|d| (d.id, d.fields.clone(), d.attempts))
                    .collect();
                plans.push(plan);
            }
        }
        let next = quadra_store::deliveries::next_at(&mut self.conn, &events)?
            .map(|s| chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S").unwrap_or(at));
        let next = match (next, batch_next) {
            (Some(a), Some(b)) => Some(a.min(b).max(at)),
            (a, b) => a.or(b),
        };
        // the tokens of the connections with consent
        for p in plans.iter_mut() {
            if let crate::deliver::Send::Http { creds, .. } = &mut p.send {
                if let Some(u) = creds.consent.as_mut() {
                    self.fill_consent(u);
                }
            }
        }
        Ok((plans, next, true))
    }

    /// `GET /v1/outbox?events=A,B&limit=50&lease=60`: up to `limit` pending events that no
    /// `outbound` delivers, each leased to the caller for `lease` seconds (10 to 3600; 60 when
    /// not given). `POST /v1/outbox/ack {"ids": […]}`: delivered. `POST /v1/outbox/nack {"id",
    /// "error", "retry_after"?, "final"?}`: not delivered; tried again after `retry_after`
    /// seconds (the usual wait when not given), or failed for good with `final`. Only those who
    /// may edit `User.roles` take the outbox: its events may carry anything the app sends out.
    fn outbox_api(
        &mut self,
        who: &Principal,
        path: &str,
        query: &HashMap<String, String>,
        body: &J,
    ) -> Result<Reply, Fail> {
        if path.starts_with("/v1/integrations") {
            return self.integrations_api(who, path, query, body);
        }
        if path.starts_with("/v1/agents") {
            return self.agents_api(who, path, body);
        }
        if path.starts_with("/v1/connections") {
            return self.connections_api(who, path, query);
        }
        if !self.who(who)?.admin {
            return Err((
                403,
                "only who may edit `User.roles` takes the outbox (a token of such a user)".into(),
            ));
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let served: Vec<String> = model.outbounds.iter().map(|o| o.event.clone()).collect();
        let at = now();
        let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let by = format!("user:{}", who.user_id);
        match path {
            "/v1/outbox" => {
                let only: Option<Vec<String>> = query.get("events").map(|e| {
                    e.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                });
                let limit = query
                    .get("limit")
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(50)
                    .clamp(1, 500);
                let lease = query
                    .get("lease")
                    .and_then(|l| l.parse::<i64>().ok())
                    .unwrap_or(60)
                    .clamp(10, 3600);
                let durable = match &self.sequencer {
                    Some(s) => s.durable().map_err(|e| (503, e))?,
                    None => u64::MAX,
                };
                let until = at + chrono::Duration::seconds(lease);
                let (now_s, until_s) = (stamp(at), stamp(until));
                let taken = self
                    .side(move |c| {
                        quadra_store::deliveries::take(
                            c,
                            &served,
                            only.as_deref(),
                            &now_s,
                            &until_s,
                            durable,
                            limit,
                        )
                    })
                    .map_err(|e| (500, e))?;
                let events: Vec<J> = taken
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id,
                            "key": crate::deliver::idempotency_key(d),
                            "event": d.event,
                            "tx": d.tx,
                            "created_at": format!("{}Z", d.created_at),
                            "attempts": d.attempts,
                            "data": crate::deliver::plain_fields(&d.fields),
                        })
                    })
                    .collect();
                Ok(self::reply(
                    200,
                    json!({ "events": events, "lease_until": format!("{}Z", stamp(until)) }),
                ))
            }
            "/v1/outbox/ack" => {
                let ids: Vec<u64> = body["ids"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                    .unwrap_or_default();
                if ids.is_empty() {
                    return Err((400, "`ids`: the events delivered".into()));
                }
                let now_s = stamp(at);
                let done = self
                    .side(move |c| {
                        let mut n = Vec::new();
                        for id in ids {
                            if quadra_store::deliveries::settle(
                                c,
                                &served,
                                id,
                                &now_s,
                                &quadra_store::deliveries::Outcome::Delivered,
                                &by,
                            )? {
                                n.push(id);
                            }
                        }
                        Ok(n)
                    })
                    .map_err(|e| (500, e))?;
                Ok(self::reply(200, json!({ "delivered": done })))
            }
            "/v1/outbox/nack" => {
                let Some(id) = body["id"].as_u64() else {
                    return Err((400, "`id`: the event not delivered".into()));
                };
                let error = body["error"]
                    .as_str()
                    .unwrap_or("refused by the integration")
                    .chars()
                    .take(1000)
                    .collect::<String>();
                let outcome = if body["final"].as_bool() == Some(true) {
                    quadra_store::deliveries::Outcome::Failed { error }
                } else {
                    let wait = body["retry_after"]
                        .as_u64()
                        .map(|s| std::time::Duration::from_secs(s.min(86_400)))
                        .unwrap_or_else(|| quadra_store::deliveries::backoff(1, 0.5));
                    quadra_store::deliveries::Outcome::Retry {
                        at: stamp(at + chrono::Duration::from_std(wait).unwrap_or_default()),
                        error,
                    }
                };
                let now_s = stamp(at);
                let changed = self
                    .side(move |c| {
                        quadra_store::deliveries::settle(c, &served, id, &now_s, &outcome, &by)
                    })
                    .map_err(|e| (500, e))?;
                if !changed {
                    return Err((
                        404,
                        format!("no pending event {id} that an integration delivers"),
                    ));
                }
                Ok(self::reply(200, json!({ "id": id, "settled": true })))
            }
            _ => Err((404, "no such path".into())),
        }
    }

    /// A message sent to `/in/<connection><path>`: its inbound found, its size and signature
    /// checked (refused: only in the log), its body kept raw with its hash, unless one with the
    /// same key came before. Read as its record when it is kept: one that does not fit is kept
    /// `rejected`, to be handled again by a deploy that reads it. The answer is sent once it is
    /// kept where the durability rule wants it.
    fn receive(
        &mut self,
        connection: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> (u16, J) {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return (503, json!({ "error": "the app is not ready" }));
        };
        let at = now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let path = if path == "/" { "" } else { path };
        let Some(i) = model
            .inbounds
            .iter()
            .find(|i| i.connection == connection && i.path.as_deref().unwrap_or("") == path)
        else {
            return (
                404,
                json!({ "error": format!("no inbound takes the messages of `{connection}` at {}", if path.is_empty() { "/" } else { path }) }),
            );
        };
        let refuse = |s: &mut Self, status: u16, why: String| {
            let (name, conn, at2, w) = (
                i.name.clone(),
                connection.to_string(),
                at.clone(),
                why.clone(),
            );
            let _ = s.side(move |c| quadra_store::deliveries::refuse(c, &name, &conn, &at2, &w));
            (status, json!({ "error": why }))
        };
        let Some(auth) = model
            .manifest
            .connection(connection)
            .and_then(|c| c.inbound.clone())
        else {
            return refuse(
                self,
                401,
                format!("`{connection}` does not say how its messages are signed"),
            );
        };
        if body.len() > auth.max_bytes {
            return refuse(
                self,
                413,
                format!("the message is larger than {} KB", auth.max_bytes / 1024),
            );
        }
        let mut secrets: Vec<String> = Vec::new();
        for v in std::iter::once(&auth.secret).chain(auth.previous.iter()) {
            if let Some(s) = model.manifest.value(&self.env, v) {
                secrets.push(s.to_string());
            }
        }
        if secrets.is_empty() {
            return refuse(
                self,
                503,
                format!("the secret of `{connection}` is not set where the app runs"),
            );
        }
        let refs: Vec<&str> = secrets.iter().map(|s| s.as_str()).collect();
        if let Err(e) = crate::inbound::verify(
            &auth.preset,
            &refs,
            headers,
            body,
            chrono::Utc::now().timestamp(),
        ) {
            return refuse(self, 401, e);
        }
        let Ok(text) = std::str::from_utf8(body) else {
            return refuse(self, 400, "the message is not text (UTF-8)".into());
        };
        let hash = crate::inbound::hash(body);
        // read as its record; the key from it (else the hash)
        let parsed = quadra_runtime::formats::read_message(model, &i.ty, text.as_bytes());
        let key = match (&parsed, &i.key) {
            (Ok(doc), Some(k)) => {
                let mut interp = quadra_runtime::Interp::new(model);
                let mut ctx = quadra_runtime::Ctx::default();
                ctx.bind(&i.binding, doc.clone());
                match interp.eval(k, &mut ctx) {
                    Ok(quadra_runtime::V::Text(t)) => t.to_string(),
                    Ok(v) => v.to_string(),
                    Err(_) => hash.clone(),
                }
            }
            _ => hash.clone(),
        };
        let marks = parsed
            .as_ref()
            .map(|d| marks_of(model, i, d))
            .unwrap_or_default();
        let error = parsed.err().map(|e| format!("{}: {e}", i.ty));
        let (name, conn, at2, body_s) = (
            i.name.clone(),
            connection.to_string(),
            at.clone(),
            text.to_string(),
        );
        let not_read = error.clone().map(|e| (key.clone(), e));
        let r = self.side(move |c| {
            quadra_store::deliveries::receive(
                c,
                &name,
                &conn,
                &key,
                &hash,
                &body_s,
                &at2,
                error.as_deref(),
                &marks,
            )
        });
        use quadra_store::deliveries::Received;
        // not its record: rejected as it came (`on OrderIn rejected as r:`)
        if let (Ok(Received::New(id)), Some((key, e))) = (&r, &not_read) {
            self.rejected(&i.name, *id, text, key, &at, 0, e, now());
        }
        match r {
            Ok(Received::New(id)) => (202, json!({ "id": id })),
            Ok(Received::Duplicate(id)) => (200, json!({ "id": id, "duplicate": true })),
            Ok(Received::Conflict(id)) => (
                409,
                json!({ "error": "a message with the same key and a different body was received before", "id": id }),
            ),
            Err(e) => (
                503,
                json!({ "error": format!("the message could not be kept: {e}") }),
            ),
        }
    }

    /// The answer of an `endpoint` to a request (`None`: no endpoint there, perhaps an
    /// inbound). The sender is checked as for an inbound. A `GET` reads its request from the
    /// query, and only reads; a `POST` reads it from the body, is kept in the inbox like a
    /// message, runs in a transaction of its own, and the same `key` gets the same answer.
    fn endpoint(
        &mut self,
        connection: &str,
        method: &str,
        path: &str,
        query: &HashMap<String, String>,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Option<(u16, J)> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref()?;
        let ep = model
            .endpoints
            .iter()
            .find(|e| e.connection == connection && e.method == method && e.path == path)?;
        let at = now();
        let stamp = at.format("%Y-%m-%dT%H:%M:%S").to_string();
        let name = format!("{method} {path}");
        let answer = |v: &quadra_runtime::V| -> (u16, J) {
            match quadra_runtime::formats::render_json(model, v) {
                Ok(t) => (200, serde_json::from_str(&t).unwrap_or(J::Null)),
                Err(e) => (
                    500,
                    json!({ "error": format!("the answer cannot be written: {e}") }),
                ),
            }
        };
        // who sent it
        let Some(auth) = model
            .manifest
            .connection(connection)
            .and_then(|c| c.inbound.clone())
        else {
            return Some((
                401,
                json!({ "error": format!("`{connection}` does not say how its requests are signed") }),
            ));
        };
        if body.len() > auth.max_bytes {
            return Some((413, json!({ "error": "the request is too large" })));
        }
        let secrets: Vec<String> = std::iter::once(&auth.secret)
            .chain(auth.previous.iter())
            .filter_map(|v| model.manifest.value(&self.env, v).map(|s| s.to_string()))
            .collect();
        let refs: Vec<&str> = secrets.iter().map(|s| s.as_str()).collect();
        if let Err(e) = crate::inbound::verify(
            &auth.preset,
            &refs,
            headers,
            body,
            chrono::Utc::now().timestamp(),
        ) {
            let (n, c, t, w) = (
                name.clone(),
                connection.to_string(),
                stamp.clone(),
                e.clone(),
            );
            let _ = self.side(move |cn| quadra_store::deliveries::refuse(cn, &n, &c, &t, &w));
            return Some((401, json!({ "error": e })));
        }
        // the request, as its record
        let text = if method == "GET" {
            let o: serde_json::Map<String, J> = query
                .iter()
                .map(|(k, v)| (k.clone(), J::String(v.clone())))
                .collect();
            J::Object(o).to_string()
        } else {
            match std::str::from_utf8(body) {
                Ok(t) => t.to_string(),
                Err(_) => {
                    return Some((400, json!({ "error": "the request is not text (UTF-8)" })))
                }
            }
        };
        let q = match quadra_runtime::formats::parse_json(model, &ep.query, &text) {
            Ok(q) => q,
            Err(e) => return Some((422, json!({ "error": format!("{}: {e}", ep.query) }))),
        };
        if method == "GET" {
            return Some(
                match quadra_store::ops::run_endpoint(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    self.sequencer.as_ref(),
                    ep,
                    q,
                    None,
                ) {
                    Ok((v, _)) => answer(&v),
                    Err(e) => (422, json!({ "error": e })),
                },
            );
        }
        // a POST: the same key, the same answer
        let hash = crate::inbound::hash(body);
        let key = match &ep.key {
            Some(k) => {
                let mut interp = quadra_runtime::Interp::new(model);
                let mut ctx = quadra_runtime::Ctx::default();
                ctx.bind(&ep.binding, q.clone());
                match interp.eval(k, &mut ctx) {
                    Ok(quadra_runtime::V::Text(t)) => t.to_string(),
                    Ok(v) => v.to_string(),
                    Err(_) => hash.clone(),
                }
            }
            None => hash.clone(),
        };
        if let Ok(Some(r)) = quadra_store::deliveries::answered(&mut self.conn, &name, &key) {
            return Some((200, serde_json::from_str(&r).unwrap_or(J::Null)));
        }
        let (n, c, t, k, h, b) = (
            name.clone(),
            connection.to_string(),
            stamp.clone(),
            key.clone(),
            hash.clone(),
            text.clone(),
        );
        let id = match self.side(move |cn| {
            quadra_store::deliveries::receive(cn, &n, &c, &k, &h, &b, &t, None, &Default::default())
        }) {
            Ok(quadra_store::deliveries::Received::New(id)) => id,
            Ok(_) => {
                return Some((
                    409,
                    json!({ "error": "this request is being handled: send it again in a moment" }),
                ))
            }
            Err(e) => {
                return Some((
                    503,
                    json!({ "error": format!("the request could not be kept: {e}") }),
                ))
            }
        };
        let r = quadra_store::ops::run_endpoint(
            &self.url,
            model,
            &checked.report,
            at,
            self.sequencer.as_ref(),
            ep,
            q,
            Some(format!("inbound:{name}#{id} handled")),
        );
        let (status, body, result): (u16, J, Result<u64, String>) = match r {
            Ok((v, tx)) => {
                let (s, b) = answer(&v);
                (s, b, Ok(tx.unwrap_or(0)))
            }
            Err(e) => (422, json!({ "error": e.clone() }), Err(e)),
        };
        let (n, t, resp) = (name.clone(), stamp.clone(), body.to_string());
        let ok = status == 200;
        let _ = self.side(move |cn| {
            quadra_store::deliveries::handled(
                cn,
                id,
                &n,
                &t,
                result.as_ref().map(|t| *t).map_err(|e| e.as_str()),
            )?;
            if ok {
                quadra_store::deliveries::respond(cn, id, &resp)?;
            }
            Ok(())
        });
        Some((status, body))
    }

    /// The pulls due now: each `inbound … pull` whose time came (or that never pulled), its
    /// url with the cursor, and when the next one is due after them.
    fn pulls(&mut self) -> (Vec<crate::deliver::Pull>, Option<chrono::NaiveDateTime>) {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return (Vec::new(), None);
        };
        let at = now();
        let mut out = Vec::new();
        let mut next: Option<chrono::NaiveDateTime> = None;
        for i in &model.inbounds {
            let Some(p) = &i.pull else { continue };
            let (cursor, next_at) = quadra_store::deliveries::pull_state(&mut self.conn, &i.name)
                .unwrap_or((None, None));
            let due = next_at
                .as_deref()
                .and_then(|n| chrono::NaiveDateTime::parse_from_str(n, "%Y-%m-%dT%H:%M:%S").ok());
            if let Some(d) = due.filter(|d| *d > at) {
                next = Some(next.map_or(d, |n| n.min(d)));
                continue;
            }
            // files, a table, a mailbox: a job, for the server or for the connection's agent
            if let Some(c) = model
                .manifest
                .connection(&i.connection)
                .filter(|c| c.pulls() != "path")
            {
                // a table whose rows of the last pulls still wait in the inbox: not pulled
                // until they are handled, so that a large table never floods it
                if c.pulls() == "table" && self.waiting(&i.name) > 2 * ROWS_PER_PULL {
                    let soon = at + chrono::Duration::seconds(2);
                    next = Some(next.map_or(soon, |n| n.min(soon)));
                    continue;
                }
                let ask = match c.pulls() {
                    "files" => crate::transport::Ask::Files {
                        pattern: p.path.clone(),
                        settle: self.settle_of(&i.name),
                        limit: 100,
                    },
                    "mailbox" => crate::transport::Ask::Mail {
                        mailbox: p.path.clone(),
                        settle: self.settle_of(&i.name),
                        limit: 100,
                        after: cursor.clone(),
                    },
                    _ => crate::transport::Ask::Rows {
                        table: p.path.clone(),
                        column: p.cursor.as_ref().and_then(|e| cursor_column(model, i, e)),
                        after: cursor.clone(),
                        limit: ROWS_PER_PULL as u32,
                    },
                };
                let consent = self.consent_of(&i.connection);
                match crate::transport::Conn::of(&model.manifest, &i.connection) {
                    Ok(conn) => out.push(crate::deliver::Pull {
                        inbound: i.name.clone(),
                        job: Some(crate::deliver::JobPull {
                            job: crate::transport::Job::Fetch { conn, ask },
                            agent: c.agent.clone(),
                            env: self.env.clone(),
                            consent,
                        }),
                        url: String::new(),
                        headers: Vec::new(),
                        creds: Default::default(),
                        timeout: std::time::Duration::from_secs(60),
                    }),
                    Err(e) => eprintln!("pull {}: {e}", i.name),
                }
                continue;
            }
            // an HTTP service of another network: its agent makes the request
            if let Some(c) = model
                .manifest
                .connection(&i.connection)
                .filter(|c| c.agent.is_some())
            {
                let path = p
                    .path
                    .replace("{cursor}", &encode(cursor.as_deref().unwrap_or("")));
                match crate::transport::Conn::of(&model.manifest, &i.connection) {
                    Ok(conn) => out.push(crate::deliver::Pull {
                        inbound: i.name.clone(),
                        job: Some(crate::deliver::JobPull {
                            job: crate::transport::Job::Fetch {
                                conn,
                                ask: crate::transport::Ask::Http { path },
                            },
                            agent: c.agent.clone(),
                            env: self.env.clone(),
                            // consent runs on the server only: refused for an agent's connection
                            consent: None,
                        }),
                        url: String::new(),
                        headers: Vec::new(),
                        creds: Default::default(),
                        timeout: std::time::Duration::from_secs(60),
                    }),
                    Err(e) => eprintln!("pull {}: {e}", i.name),
                }
                continue;
            }
            match crate::deliver::connection_request(&model.manifest, &self.env, &i.connection) {
                Ok((base, headers, mut creds, timeout)) => {
                    if let Some(u) = creds.consent.as_mut() {
                        self.fill_consent(u);
                    }
                    let path = p
                        .path
                        .replace("{cursor}", &encode(cursor.as_deref().unwrap_or("")));
                    out.push(crate::deliver::Pull {
                        inbound: i.name.clone(),
                        job: None,
                        url: format!("{base}{path}"),
                        headers,
                        creds,
                        timeout,
                    })
                }
                Err(e) => eprintln!("pull {}: {e}", i.name),
            }
        }
        // a message to try again (`retry for`) is due then too
        if let Some(n) = quadra_store::deliveries::inbox_next_at(&mut self.conn)
            .ok()
            .flatten()
            .and_then(|n| chrono::NaiveDateTime::parse_from_str(&n, "%Y-%m-%dT%H:%M:%S").ok())
        {
            next = Some(next.map_or(n, |x| x.min(n)));
        }
        (out, next)
    }

    /// `/v1/agents`: the on-premise agents (admins only). `GET` lists them; `POST {name}` adds
    /// one (or adds it again) and answers its one-time code, valid an hour; `POST
    /// /v1/agents/<name>/revoke` revokes it (the host then drops what it waits for).
    fn agents_api(&mut self, who: &Principal, path: &str, body: &J) -> Result<Reply, Fail> {
        if !self.who(who)?.admin {
            return Err((
                403,
                "only who may edit `User.roles` manages the agents".into(),
            ));
        }
        let parts: Vec<&str> = path
            .trim_start_matches("/v1/agents")
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        let at = now();
        let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let by = format!("user:{}", who.user_id);
        match parts.as_slice() {
            [] if body.is_null() => {
                let rows = quadra_store::agents::list(&mut self.conn).map_err(|e| (500, e))?;
                let z = |o: &Option<String>| o.as_ref().map(|t| format!("{t}Z"));
                Ok(self::reply(
                    200,
                    json!({
                        "agents": rows.iter().map(|a| json!({
                            "name": a.name, "created_at": format!("{}Z", a.created_at), "created_by": a.created_by,
                            "registered_at": z(&a.registered_at), "revoked_at": z(&a.revoked_at),
                            "code_expires": z(&a.code_expires), "version": a.version, "seen_at": z(&a.seen_at),
                        })).collect::<Vec<_>>()
                    }),
                ))
            }
            [] => {
                let name = body["name"].as_str().unwrap_or("").to_string();
                let ok = (1..=40).contains(&name.len())
                    && name.starts_with(|c: char| c.is_ascii_lowercase())
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
                if !ok {
                    return Err((400, "an agent's name is in lower case letters, digits and `_`, as in `location = \"agent:<name>\"` (`milano`)".into()));
                }
                let code = quadra_store::agents::new_code();
                let expires = stamp(at + chrono::Duration::hours(1));
                let (n, c, e, t) = (name.clone(), code.clone(), expires.clone(), stamp(at));
                self.side(move |cn| quadra_store::agents::add(cn, &n, &c, &e, &t, &by))
                    .map_err(|e| (500, e))?;
                Ok(self::reply(
                    200,
                    json!({ "name": name, "code": code, "expires_at": format!("{expires}Z") }),
                ))
            }
            [name, "revoke"] => {
                let (n, t) = (name.to_string(), stamp(at));
                let revoked = self
                    .side(move |cn| quadra_store::agents::revoke(cn, &n, &t))
                    .map_err(|e| (500, e))?;
                if !revoked {
                    return Err((404, format!("no agent `{name}` to revoke")));
                }
                Ok(self::reply(200, json!({ "name": name, "revoked": true })))
            }
            _ => Err((404, "no such agents call".into())),
        }
    }

    /// An agent registers with its one-time code: its name and a new key, of which only the
    /// hash is kept.
    fn agent_hello(&mut self, code: &str, version: &str) -> Result<(String, String), String> {
        let key = format!("qa_{}", agent_secret(32));
        let (c, k, v, t) = (
            code.trim().to_ascii_uppercase(),
            key.clone(),
            version.to_string(),
            now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        );
        match self.side(move |cn| quadra_store::agents::register(cn, &c, &k, &v, &t))? {
            Some(name) => Ok((name, key)),
            None => Err("the code is not valid: unknown, used already or expired (an hour)".into()),
        }
    }

    /// The agent whose key this is (registered, not revoked); when it was seen is written at
    /// most once a minute.
    fn agent_key(&mut self, key: &str, version: &str) -> Option<String> {
        let name = quadra_store::agents::by_key(&mut self.conn, key)
            .ok()
            .flatten()?;
        let due = self
            .agents_seen
            .get(&name)
            .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(60));
        if due {
            self.agents_seen
                .insert(name.clone(), std::time::Instant::now());
            let (n, v, t) = (
                name.clone(),
                version.to_string(),
                now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            );
            let _ = self.side(move |cn| quadra_store::agents::seen(cn, &n, &v, &t));
        }
        Some(name)
    }

    /// How many messages of `inbound` wait to be handled (counted up to a little more than two
    /// pages of a table).
    fn waiting(&mut self, inbound: &str) -> usize {
        self.conn
            .query(
                "SELECT 1 FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"state\" = 'received' LIMIT 2001",
                &[quadra_store::conn::Sql::Text(inbound.to_string())],
            )
            .map(|r| r.len())
            .unwrap_or(0)
    }

    /// The messages of `inbound` handled for good, to settle on the other side at its next pull.
    fn settle_of(&mut self, inbound: &str) -> Vec<crate::transport::Settle> {
        quadra_store::deliveries::to_settle(&mut self.conn, inbound, 100)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, ok, report)| crate::transport::Settle { name, ok, report })
            .collect()
    }

    /// What a pull of `inbound` brought: each message kept in the inbox like one sent in (its
    /// key, its hash; read as its record, else `rejected`), the cursor moved to the largest of
    /// theirs, and the time of the next pull, in one write. A failed pull is tried again sooner
    /// (or after `Retry-After`). Returns how many messages are new.
    fn pulled(&mut self, inbound: &str, result: crate::deliver::PullResult) -> usize {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return 0;
        };
        let Some(i) = model.inbounds.iter().find(|i| i.name == inbound) else {
            return 0;
        };
        let Some(p) = &i.pull else { return 0 };
        let at = now();
        let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
        let every = match quadra_store::ops::eval_fixed(model, &p.every) {
            Ok(quadra_runtime::V::Duration(s)) => s.max(60),
            _ => 300,
        };
        let (name, conn) = (i.name.clone(), i.connection.clone());
        let fail = |s: &mut Self, why: String, after: Option<u64>| {
            let wait = after.unwrap_or((every as u64).min(300)).clamp(5, 3600);
            let (n, t, nx) = (
                name.clone(),
                stamp(at),
                stamp(at + chrono::Duration::seconds(wait as i64)),
            );
            let _ =
                s.side(move |c| quadra_store::deliveries::pulled(c, &n, &t, &nx, None, Some(&why)));
            0
        };
        // what came, each with its origin on the other side, as bytes; and what was settled
        let mut items: Vec<(Option<String>, Vec<u8>, Option<String>)> = Vec::new();
        let mut settled: Vec<String> = Vec::new();
        let (status, body, retry_after) = match result {
            crate::deliver::PullResult::Http(Ok(r)) => r,
            crate::deliver::PullResult::Http(Err(e)) => return fail(self, e, None),
            crate::deliver::PullResult::Job(crate::transport::Outcome::Fetched(f)) => {
                for it in f.items {
                    items.push((Some(it.name), it.body, it.cursor));
                }
                settled = f.settled;
                (200, String::new(), None)
            }
            crate::deliver::PullResult::Job(crate::transport::Outcome::Failed(f)) => {
                return fail(self, f.text().to_string(), None)
            }
            // an HTTP pull an agent made: its answer, as the server's own
            crate::deliver::PullResult::Job(crate::transport::Outcome::Answered {
                status,
                body,
                retry_after,
            }) => (status, body, retry_after),
            crate::deliver::PullResult::Job(crate::transport::Outcome::Put) => {
                return fail(self, "the job answered with no messages".into(), None)
            }
        };

        let is_job =
            matches!(model.manifest.connection(&i.connection), Some(c) if c.pulls() != "path");
        if !is_job && !(200..300).contains(&status) {
            let text: String = body.chars().take(200).collect();
            return fail(
                self,
                format!("the pull answered {status}: {}", text.trim()),
                retry_after,
            );
        }
        let doc: J = if is_job {
            J::Null
        } else {
            match serde_json::from_str(&body) {
                Ok(d) => d,
                Err(e) => return fail(self, format!("the answer is not JSON: {e}"), None),
            }
        };
        // the messages: the list in the page (`items Page.field`), or the answer itself
        let list = match &p.items {
            Some((page, field)) => {
                let keys = model
                    .entity(page)
                    .and_then(|e| model.find_field(e, field))
                    .map(quadra_ir::formats::place)
                    .unwrap_or_else(|| vec![field.clone()]);
                let mut cur = Some(&doc);
                for k in &keys {
                    cur = cur.and_then(|c| c.get(k));
                }
                cur.cloned()
            }
            None => Some(doc.clone()),
        };
        if !is_job {
            let Some(J::Array(list)) = list else {
                return fail(
                    self,
                    "the answer has no list of messages where the inbound says".into(),
                    None,
                );
            };
            items = list
                .into_iter()
                .map(|j| (None, j.to_string().into_bytes(), None))
                .collect();
        }
        // the rows of a table: each field of the record has its column
        let table =
            matches!(model.manifest.connection(&i.connection), Some(c) if c.pulls() == "table");
        if let Some(why) = items
            .first()
            .filter(|_| table)
            .and_then(|(_, b, _)| missing_column(model, i, &p.path, b))
        {
            return fail(self, why, None);
        }
        let (old_cursor, _) =
            quadra_store::deliveries::pull_state(&mut self.conn, &name).unwrap_or((None, None));
        let full = table && items.len() >= ROWS_PER_PULL;
        let mut kept: Vec<(
            String,
            String,
            String,
            Option<String>,
            quadra_store::deliveries::Marks,
        )> = Vec::new();
        // the rows of the cursor's last value, read again (`>=`): dropped quietly when kept
        let mut at_cursor: Vec<bool> = Vec::new();
        let mut cursor: Option<String> = None;
        for (origin, bytes, item_cursor) in items {
            let (text, base64) = crate::transport::body_text(&bytes);
            let hash = crate::inbound::hash(&bytes);
            // a file of rows: only what is wrong with the whole file refuses it here; its
            // rows are reported when it is handled
            let parsed = if crate::files::rows_record(model, &i.ty) {
                quadra_runtime::formats::read_rows(model, &i.ty, &bytes).map(|each| {
                    quadra_runtime::V::List(std::rc::Rc::new(
                        each.into_iter().filter_map(|(_, r)| r.ok()).collect(),
                    ))
                })
            } else {
                quadra_runtime::formats::read_message(model, &i.ty, &bytes)
            };
            let eval = |e: &quadra_ir::Expr, doc: &quadra_runtime::V| -> Option<String> {
                let mut interp = quadra_runtime::Interp::new(model);
                let mut ctx = quadra_runtime::Ctx::default();
                ctx.bind(&i.binding, doc.clone());
                interp.eval(e, &mut ctx).ok().map(|v| match v {
                    quadra_runtime::V::Text(t) => t.to_string(),
                    quadra_runtime::V::DateTime(t) => t.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    other => other.to_string(),
                })
            };
            // the key: the inbound's, else (from the other side) its name and its hash, else the hash
            let key = match (&parsed, &i.key, &origin) {
                (Ok(d), Some(k), _) => eval(k, d).unwrap_or_else(|| hash.clone()),
                (_, _, Some(o)) => format!("{o}#{}", &hash[..16]),
                _ => hash.clone(),
            };
            // the rows of a table come in the order of their cursor (a number does not sort as
            // text): the last one's is where the next pull starts
            at_cursor.push(item_cursor.is_some() && item_cursor == old_cursor);
            if let Some(v) = item_cursor {
                cursor = Some(v);
            } else if let (Ok(d), Some(c)) = (&parsed, &p.cursor) {
                if let Some(v) = eval(c, d) {
                    if cursor.as_deref().is_none_or(|old| v.as_str() > old) {
                        cursor = Some(v);
                    }
                }
            }
            let mut marks = parsed
                .as_ref()
                .map(|d| marks_of(model, i, d))
                .unwrap_or_default();
            marks.origin = origin;
            marks.base64 = base64;
            let error = parsed.err().map(|e| format!("{}: {e}", i.ty));
            kept.push((key, hash, text, error, marks));
        }
        let (n, c, t, mut nx) = (
            name.clone(),
            conn.clone(),
            stamp(at),
            stamp(at + chrono::Duration::seconds(every)),
        );
        // a full page of a table that moved the cursor: more rows wait, the next pull is at
        // once (held back while the inbox has two pages of them to handle: `pulls`)
        if full && cursor.is_some() && cursor != old_cursor {
            nx = t.clone();
        }
        let r = self.side(move |cn| {
            let mut new = 0;
            let mut again = false;
            for (k, (key, hash, text, error, marks)) in kept.iter().enumerate() {
                // a row read again at the cursor (`>=`), kept already: quietly not again
                if at_cursor[k]
                    && !cn
                        .query(
                            "SELECT 1 FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"key\" = ?text AND \"hash\" = ?text LIMIT 1",
                            &[
                                quadra_store::conn::Sql::Text(n.clone()),
                                quadra_store::conn::Sql::Text(key.clone()),
                                quadra_store::conn::Sql::Text(hash.clone()),
                            ],
                        )?
                        .is_empty()
                {
                    continue;
                }
                match quadra_store::deliveries::receive(
                    cn,
                    &n,
                    &c,
                    key,
                    hash,
                    text,
                    &t,
                    error.as_deref(),
                    marks,
                )? {
                    quadra_store::deliveries::Received::New(_) => new += 1,
                    // a file that came again as it was: moved there again, not handled again
                    quadra_store::deliveries::Received::Duplicate(id) if marks.origin.is_some() => {
                        quadra_store::deliveries::unsettle(cn, id)?;
                        again = true;
                    }
                    _ => {}
                }
            }
            quadra_store::deliveries::settled(cn, &n, &settled, &t)?;
            quadra_store::deliveries::pulled(cn, &n, &t, &nx, cursor.as_deref(), None)?;
            if again {
                quadra_store::deliveries::pull_soon(cn, &n, &t)?;
            }
            Ok(new)
        });
        r.unwrap_or_else(|e| {
            eprintln!("pull {inbound}: the messages were not kept: {e}");
            0
        })
    }

    /// Up to `limit` messages of the inbox handled, oldest first, each in a transaction of its
    /// own with the cause `inbound:Name#id handled`; one that fails is `rejected` with the
    /// error. Returns how many, and whether the model has any inbound.
    fn handle_inbox(&mut self, limit: usize) -> (usize, bool) {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return (0, false);
        };
        if model.inbounds.is_empty() {
            return (0, false);
        }
        let now_s = now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let messages = match quadra_store::deliveries::to_handle(&mut self.conn, &now_s, limit) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("inbox: {e}");
                return (0, true);
            }
        };
        let n = messages.len();
        // the units, currencies and company, read once for the messages (the engine keeps them
        // while none of those records is written)
        let units = if messages.is_empty() {
            None
        } else {
            let _ = self.interp(model, None, None);
            self.units.as_ref().map(|(u, c, _)| (u.clone(), *c))
        };
        for m in messages {
            let at = now();
            let stamp = at.format("%Y-%m-%dT%H:%M:%S").to_string();
            let name = m.inbound.clone();
            let inbound = model.inbounds.iter().find(|i| i.name == m.inbound);
            // `latest by … using`: one as new or newer handled already, this one is superseded
            if let (Some(_), Some(latest), Some(version)) = (
                inbound.and_then(|i| i.latest.as_ref()),
                &m.marks.latest,
                &m.marks.version,
            ) {
                let newer =
                    quadra_store::deliveries::handled_versions(&mut self.conn, &name, latest)
                        .unwrap_or_default()
                        .into_iter()
                        .find(|(_, v)| quadra_store::deliveries::as_new(v, version));
                if let Some((by, _)) = newer {
                    let id = m.id;
                    let _ = self.side(move |c| {
                        quadra_store::deliveries::inbox_supersede(c, id, &name, &stamp, by)
                    });
                    continue;
                }
            }
            // the report of the rows of a file: those refused, and why
            let mut rows_report: Option<String> = None;
            let mark = format!("inbound:{}#{} ", m.inbound, m.id);
            if self.inbox_recent.is_none() {
                let recent = self
                    .conn
                    .query(
                        "SELECT \"cause\" FROM \"_quadra_commits\" ORDER BY \"tx\" DESC LIMIT 1000",
                        &[],
                    )
                    .unwrap_or_default();
                self.inbox_recent = Some(recent.into_iter().filter_map(|r| r[0].clone()).collect());
            }
            let maybe_handled = self
                .inbox_recent
                .as_ref()
                .is_some_and(|r| r.iter().any(|c| c.contains(&mark)));
            let result: Result<u64, String> = (|| {
                let i = model
                    .inbounds
                    .iter()
                    .find(|i| i.name == m.inbound)
                    .ok_or_else(|| {
                        format!("no inbound `{}` in the code deployed now", m.inbound)
                    })?;
                // handled already (the server stopped before writing it): done
                if maybe_handled {
                    if let Some(tx) =
                        quadra_store::deliveries::handled_by(&mut self.conn, &i.name, m.id)?
                    {
                        return Ok(tx);
                    }
                }
                let bytes = crate::transport::body_bytes(&m.body, m.marks.base64);
                // a file of rows: all of them or none, or each row that works
                if crate::files::rows_record(model, &i.ty) {
                    let (r, report) = crate::files::handle_file(
                        &mut self.conn,
                        &self.url,
                        self.sequencer.as_ref(),
                        model,
                        &checked.report,
                        at,
                        i,
                        m.id,
                        &bytes,
                    );
                    rows_report = report;
                    return r;
                }
                let doc = quadra_runtime::formats::read_message(model, &i.ty, &bytes)
                    .map_err(|e| format!("{}: {e}", i.ty))?;
                quadra_store::ops::handle_inbound(
                    &self.url,
                    model,
                    &checked.report,
                    at,
                    self.sequencer.as_ref(),
                    i,
                    doc,
                    format!("inbound:{}#{} handled", i.name, m.id),
                    Some(self.schema.clone()),
                    units.as_ref().map(|(u, c)| (u, *c)),
                )
                .map(|tx| tx.unwrap_or(0))
            })();
            if let Err(e) = &result {
                eprintln!("inbound {name}#{}: {e}", m.id);
            }
            let id = m.id;
            // `retry for`: tried again later, while the time lasts
            let mut result = result;
            if let (Err(e), Some(i)) = (&result, inbound) {
                let seconds = |e: &quadra_ir::Expr| match quadra_store::ops::eval_fixed(model, e) {
                    Ok(quadra_runtime::V::Duration(s)) => Some(s.max(1) as u64),
                    _ => None,
                };
                if let Some(for_s) = i.retry_for.as_ref().and_then(seconds) {
                    let came =
                        chrono::NaiveDateTime::parse_from_str(&m.received_at, "%Y-%m-%dT%H:%M:%S")
                            .unwrap_or(at);
                    let (mut first, mut max) = (30, 3600);
                    if let Some(quadra_ir::Expr {
                        kind: quadra_ir::ExprKind::Binary { lhs, rhs, .. },
                        ..
                    }) = &i.backoff
                    {
                        if let (Some(a), Some(b)) = (seconds(lhs), seconds(rhs)) {
                            (first, max) = (a, b);
                        }
                    }
                    let jitter =
                        ((m.id.wrapping_mul(2654435761) ^ m.attempts) % 1000) as f64 / 1000.0;
                    let wait = quadra_store::deliveries::backoff_between(
                        m.attempts + 1,
                        jitter,
                        first,
                        max,
                    );
                    let next = at + chrono::Duration::from_std(wait).unwrap_or_default();
                    let until =
                        came + chrono::Duration::seconds(for_s.min(10 * 365 * 86_400) as i64);
                    if next <= until {
                        let (e, nx) = (e.clone(), next.format("%Y-%m-%dT%H:%M:%S").to_string());
                        let _ = self.side(move |c| {
                            quadra_store::deliveries::inbox_later(c, id, &name, &stamp, &e, &nx)
                        });
                        continue;
                    }
                    result = Err(format!("{e} (tried for {})", crate::deliver::human(for_s)));
                }
            }
            let rejected = result.as_ref().err().cloned();
            let inbound_name = name.clone();
            let from_there = m.marks.origin.is_some();
            let marked = self.side(move |c| {
                quadra_store::deliveries::handled(
                    c,
                    id,
                    &name,
                    &stamp,
                    result.as_ref().map(|t| *t).map_err(|e| e.as_str()),
                )?;
                if let Some(r) = &rows_report {
                    quadra_store::deliveries::set_report(c, id, r)?;
                }
                // a file handled is moved on the other side soon, not at the next pull
                if from_there {
                    quadra_store::deliveries::pull_soon(c, &name, &stamp)?;
                }
                Ok(())
            });
            // not marked: looked for in the log before it is handled again
            if marked.is_err() {
                if let Some(r) = self.inbox_recent.as_mut() {
                    r.push(mark);
                }
            }
            // `on OrderIn rejected as r:`: someone to tell, a task to open
            if let Some(error) = rejected {
                self.rejected(
                    &inbound_name,
                    m.id,
                    &m.body,
                    &m.key,
                    &m.received_at,
                    m.attempts + 1,
                    &error,
                    at,
                );
            }
        }
        (n, true)
    }

    /// The reactions to a message rejected for good (`on OrderIn rejected as r:`), in a
    /// transaction of their own with the cause `inbound:OrderIn#id rejected`.
    #[allow(clippy::too_many_arguments)]
    fn rejected(
        &mut self,
        inbound: &str,
        id: u64,
        body: &str,
        key: &str,
        received_at: &str,
        attempts: u64,
        error: &str,
        at: chrono::NaiveDateTime,
    ) {
        use quadra_runtime::V;
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return;
        };
        let want = quadra_ir::Trigger::Rejected;
        if !model
            .reactions
            .iter()
            .any(|r| r.entity == inbound && r.trigger == want)
        {
            return;
        }
        let came =
            chrono::NaiveDateTime::parse_from_str(received_at, "%Y-%m-%dT%H:%M:%S").unwrap_or(at);
        let text = |s: &str| V::Text(std::rc::Rc::from(s));
        let value = V::Tuple(std::rc::Rc::new(vec![
            (Some("message".to_string()), text(body)),
            (Some("key".to_string()), text(key)),
            (Some("at".to_string()), V::DateTime(came)),
            (Some("attempts".to_string()), V::Int(attempts as i64)),
            (Some("error".to_string()), text(error)),
        ]));
        if let Err(e) = quadra_store::ops::outcome_reactions(
            &self.url,
            model,
            &checked.report,
            at,
            self.sequencer.as_ref(),
            inbound,
            want,
            value,
            format!("inbound:{inbound}#{id} rejected"),
        ) {
            eprintln!("inbound {inbound}#{id}: the reaction failed: {e}");
        }
    }

    /// The reactions to an event not sent (`on E expired as x:`, `on E skipped as x:`), in a
    /// transaction of their own with the cause `outbound:E#id expired`: `x.event`, `x.at`,
    /// `x.attempts`, `x.reason`. One that fails goes to the event's log.
    fn not_sent(
        &mut self,
        id: u64,
        event: &str,
        want: quadra_ir::Trigger,
        fields: &J,
        attempts: u64,
        reason: &str,
    ) {
        use quadra_runtime::V;
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return;
        };
        if !model
            .reactions
            .iter()
            .any(|r| r.entity == event && r.trigger == want)
        {
            return;
        }
        let at = now();
        let event_fields: Vec<(Option<String>, V)> = fields
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (Some(k.clone()), quadra_runtime::codec::from_json(v)))
                    .collect()
            })
            .unwrap_or_default();
        let value = V::Tuple(std::rc::Rc::new(vec![
            (
                Some("event".to_string()),
                V::Tuple(std::rc::Rc::new(event_fields)),
            ),
            (Some("at".to_string()), V::DateTime(at)),
            (Some("attempts".to_string()), V::Int(attempts as i64)),
            (
                Some("reason".to_string()),
                V::Text(std::rc::Rc::from(reason)),
            ),
        ]));
        let word = if want == quadra_ir::Trigger::Expired {
            "expired"
        } else {
            "skipped"
        };
        let r = quadra_store::ops::outcome_reactions(
            &self.url,
            model,
            &checked.report,
            at,
            self.sequencer.as_ref(),
            event,
            want,
            value,
            format!("outbound:{event}#{id} {word}"),
        );
        if let Err(e) = r {
            eprintln!("outbound {event}#{id}: the reaction failed: {e}");
            let (ev, stamp) = (
                event.to_string(),
                at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            );
            let _ =
                self.side(move |c| quadra_store::deliveries::log_reaction(c, id, &ev, &stamp, &e));
        }
    }

    /// The reactions to the final outcome of event `id` (`on E delivered as d:`, `on E failed as
    /// f:`), in a transaction of their own with the cause `outbound:E#id`. One that fails leaves
    /// the outcome as it is (it is true: the receiver got it, or not) and goes to the event's log.
    #[allow(clippy::too_many_arguments)]
    fn outcome_reactions(
        &mut self,
        id: u64,
        event: &str,
        outcome: &quadra_store::deliveries::Outcome,
        attempt: &quadra_store::deliveries::Attempt,
        fields: &J,
        attempts: u64,
        answer: Option<&str>,
    ) {
        use quadra_runtime::V;
        use quadra_store::deliveries::Outcome;
        let (delivered, error) = match outcome {
            Outcome::Delivered => (true, None),
            Outcome::Failed { error } => (false, Some(error.clone())),
            Outcome::Retry { .. } => return,
        };
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return;
        };
        let at = now();
        let text = |s: &str| V::Text(std::rc::Rc::from(s));
        let event_fields: Vec<(Option<String>, V)> = fields
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (Some(k.clone()), quadra_runtime::codec::from_json(v)))
                    .collect()
            })
            .unwrap_or_default();
        let mut parts = vec![
            (
                Some("event".to_string()),
                V::Tuple(std::rc::Rc::new(event_fields)),
            ),
            (Some("at".to_string()), V::DateTime(at)),
            (Some("attempts".to_string()), V::Int(attempts as i64)),
            (
                Some("status".to_string()),
                attempt.status.map_or(V::None, |s| V::Int(s as i64)),
            ),
            (
                Some("response".to_string()),
                attempt.excerpt.as_deref().map_or(V::None, text),
            ),
        ];
        if let Some(e) = &error {
            parts.push((Some("error".to_string()), text(e)));
        }
        // `reply T`: the answer read as a record `from json` or `from xml` (none if it does not
        // fit, said in the log)
        let mut reply_error = None;
        if let Some(t) = model
            .outbounds
            .iter()
            .find(|o| o.event == event)
            .and_then(|o| o.reply.clone())
        {
            let reply = match answer.map(|a| quadra_runtime::formats::parse(model, &t, a)) {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    reply_error = Some(format!("the answer is not a {t}: {e}"));
                    V::None
                }
                None => V::None,
            };
            parts.push((Some("reply".to_string()), reply));
        }
        let value = V::Tuple(std::rc::Rc::new(parts));
        if let Some(e) = reply_error {
            let (ev, stamp) = (
                event.to_string(),
                at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            );
            let _ =
                self.side(move |c| quadra_store::deliveries::log_reaction(c, id, &ev, &stamp, &e));
        }
        let r = quadra_store::ops::outcome_reactions(
            &self.url,
            model,
            &checked.report,
            at,
            self.sequencer.as_ref(),
            event,
            if delivered {
                quadra_ir::Trigger::Delivered
            } else {
                quadra_ir::Trigger::Failed
            },
            value,
            format!(
                "outbound:{event}#{id} {}",
                if delivered { "delivered" } else { "failed" }
            ),
        );
        if let Err(e) = r {
            eprintln!("outbound {event}#{id}: the reaction failed: {e}");
            let (ev, stamp) = (
                event.to_string(),
                at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            );
            let _ =
                self.side(move |c| quadra_store::deliveries::log_reaction(c, id, &ev, &stamp, &e));
        }
    }

    /// `/v1/integrations`: what came in and went out, for the studio's page "Integrations".
    /// Only those who may edit `User.roles`.
    ///
    /// - `GET /v1/integrations`: each outbound and inbound with its connection, how many
    ///   messages are in each state, and the time of the oldest still waiting
    /// - `GET /v1/integrations/outbox?state=&event=&limit=` and `…/inbox?state=&inbound=&limit=`:
    ///   the newest first
    /// - `GET /v1/integrations/outbox/{id}`, `…/inbox/{id}`: one, with its log (and its body)
    /// - `POST /v1/integrations/outbox/{id}/retry`, `…/skip {"reason"}` (and the same for the
    ///   inbox): with who and why in the log
    fn integrations_api(
        &mut self,
        who: &Principal,
        path: &str,
        query: &HashMap<String, String>,
        body: &J,
    ) -> Result<Reply, Fail> {
        if !self.who(who)?.admin {
            return Err((
                403,
                "only who may edit `User.roles` sees the integrations".into(),
            ));
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let by = format!("user:{}", who.user_id);
        let at = now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let parts: Vec<&str> = path
            .trim_start_matches("/v1/integrations")
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        let step = |s: &quadra_store::deliveries::Step| {
            json!({ "at": format!("{}Z", s.at), "step": s.step, "outcome": s.outcome, "connection": s.connection,
                    "status": s.status, "duration_ms": s.duration_ms, "error": s.error, "excerpt": s.excerpt,
                    "by": s.by, "reason": s.reason })
        };
        let limit = query
            .get("limit")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(100)
            .clamp(1, 1000);
        let e500 = |e: String| (500, e);
        match parts.as_slice() {
            [] => {
                let events = self.outbox_rows().map_err(e500)?;
                let messages =
                    quadra_store::deliveries::inbox(&mut self.conn, None).map_err(e500)?;
                let summary = |states: Vec<(&str, &str)>| {
                    let mut by_state: std::collections::BTreeMap<String, u64> = Default::default();
                    let mut oldest: Option<String> = None;
                    for (s, at) in states {
                        *by_state.entry(s.to_string()).or_default() += 1;
                        if matches!(s, "pending" | "received")
                            && oldest.as_deref().is_none_or(|o| at < o)
                        {
                            oldest = Some(at.to_string());
                        }
                    }
                    json!({ "states": by_state, "oldest_waiting": oldest.map(|o| format!("{o}Z")) })
                };
                let outbounds: Vec<J> = model
                    .outbounds
                    .iter()
                    .map(|o| {
                        let mine: Vec<(&str, &str)> = events
                            .iter()
                            .filter(|e| e.event == o.event)
                            .map(|e| (e.state.as_str(), e.created_at.as_str()))
                            .collect();
                        let mut j = summary(mine);
                        j["event"] = json!(o.event);
                        j["connection"] = json!(o.connection);
                        j
                    })
                    .collect();
                let inbounds: Vec<J> = model
                    .inbounds
                    .iter()
                    .map(|i| {
                        let mine: Vec<(&str, &str)> = messages
                            .iter()
                            .filter(|m| m.inbound == i.name)
                            .map(|m| (m.state.as_str(), m.received_at.as_str()))
                            .collect();
                        let mut j = summary(mine);
                        j["inbound"] = json!(i.name);
                        j["connection"] = json!(i.connection);
                        j["path"] = json!(format!(
                            "/in/{}{}",
                            i.connection,
                            i.path.as_deref().unwrap_or("")
                        ));
                        j
                    })
                    .collect();
                let endpoints: Vec<J> = model.endpoints.iter().map(|e| json!({ "method": e.method, "path": format!("/in/{}{}", e.connection, e.path), "connection": e.connection })).collect();
                let connections = self.connections_status();
                Ok(self::reply(
                    200,
                    json!({ "outbounds": outbounds, "inbounds": inbounds, "endpoints": endpoints, "connections": connections }),
                ))
            }
            ["outbox"] => {
                let mut events = self.outbox_rows().map_err(e500)?;
                events.retain(|e| {
                    query.get("state").is_none_or(|s| &e.state == s)
                        && query.get("event").is_none_or(|n| &e.event == n)
                });
                events.reverse();
                events.truncate(limit);
                let list: Vec<J> = events.iter().map(|e| json!({
                    "id": e.id, "event": e.event, "tx": e.tx, "state": e.state, "created_at": format!("{}Z", e.created_at),
                    "attempts": e.attempts, "next_at": e.next_at.as_ref().map(|n| format!("{n}Z")), "error": e.error,
                    "delivered_at": e.delivered_at.as_ref().map(|n| format!("{n}Z")), "data": crate::deliver::plain_fields(&e.fields),
                })).collect();
                Ok(self::reply(200, json!({ "events": list })))
            }
            ["outbox", id] => {
                let id: u64 = id.parse().map_err(|_| (404, "no such event".to_string()))?;
                let e = self
                    .outbox_rows()
                    .map_err(e500)?
                    .into_iter()
                    .find(|e| e.id == id)
                    .ok_or((404, "no such event".to_string()))?;
                let log: Vec<J> = quadra_store::deliveries::log(&mut self.conn, id)
                    .map_err(e500)?
                    .iter()
                    .map(step)
                    .collect();
                Ok(self::reply(
                    200,
                    json!({
                        "id": e.id, "event": e.event, "tx": e.tx, "state": e.state, "created_at": format!("{}Z", e.created_at),
                        "attempts": e.attempts, "next_at": e.next_at.as_ref().map(|n| format!("{n}Z")), "error": e.error,
                        "data": crate::deliver::plain_fields(&e.fields), "log": log,
                    }),
                ))
            }
            ["outbox", id, action] | ["inbox", id, action]
                if matches!(*action, "retry" | "skip") =>
            {
                let id: u64 = id
                    .parse()
                    .map_err(|_| (404, "no such message".to_string()))?;
                let reason = body["reason"].as_str().unwrap_or("").to_string();
                let out = parts[0] == "outbox";
                let act = action.to_string();
                let n = self
                    .side(move |c| match (out, act.as_str()) {
                        (true, "retry") => quadra_store::deliveries::retry(c, &[id], &at, &by),
                        (true, _) => quadra_store::deliveries::skip(c, &[id], &at, &by, &reason),
                        (false, "retry") => {
                            quadra_store::deliveries::inbox_retry(c, &[id], &at, &by)
                        }
                        (false, _) => {
                            quadra_store::deliveries::inbox_skip(c, &[id], &at, &by, &reason)
                        }
                    })
                    .map_err(|e| (400, e))?;
                if n == 0 {
                    return Err((
                        409,
                        format!("{} {id} is not in a state that allows `{action}`", parts[0]),
                    ));
                }
                Ok(self::reply(200, json!({ "id": id, "done": action })))
            }
            ["inbox"] => {
                let mut messages = quadra_store::deliveries::inbox(
                    &mut self.conn,
                    query.get("state").map(|s| s.as_str()),
                )
                .map_err(e500)?;
                messages.retain(|m| query.get("inbound").is_none_or(|n| &m.inbound == n));
                messages.reverse();
                messages.truncate(limit);
                let list: Vec<J> = messages.iter().map(|m| json!({
                    "id": m.id, "inbound": m.inbound, "connection": m.connection, "key": m.key, "state": m.state,
                    "received_at": format!("{}Z", m.received_at), "attempts": m.attempts, "error": m.error, "tx": m.tx,
                })).collect();
                Ok(self::reply(200, json!({ "messages": list })))
            }
            ["inbox", id] => {
                let id: u64 = id
                    .parse()
                    .map_err(|_| (404, "no such message".to_string()))?;
                let m = quadra_store::deliveries::inbox(&mut self.conn, None)
                    .map_err(e500)?
                    .into_iter()
                    .find(|m| m.id == id)
                    .ok_or((404, "no such message".to_string()))?;
                let log: Vec<J> = quadra_store::deliveries::inbox_log(&mut self.conn, id)
                    .map_err(e500)?
                    .iter()
                    .map(step)
                    .collect();
                Ok(self::reply(
                    200,
                    json!({
                        "id": m.id, "inbound": m.inbound, "connection": m.connection, "key": m.key, "hash": m.hash, "state": m.state,
                        "received_at": format!("{}Z", m.received_at), "attempts": m.attempts, "error": m.error, "tx": m.tx,
                        "body": m.body, "log": log,
                    }),
                ))
            }
            _ => Err((404, "no such path".into())),
        }
    }

    /// The events of the outbox, oldest first, on this engine's connection.
    fn outbox_rows(&mut self) -> Result<Vec<quadra_store::OutboxEvent>, String> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut db = Database::open(&self.url, model, &checked.report)?;
        db.outbox(false)
    }

    /// Where an interpreter's commits go: the tenant's sequencer, or the database directly.
    pub(crate) fn sink<'m>(
        &self,
        db: Database,
        model: &'m quadra_ir::Model,
    ) -> Box<dyn quadra_runtime::CommitSink + 'm> {
        match &self.sequencer {
            Some(s) => Box::new(quadra_store::SequencedSink {
                db,
                model,
                sequencer: s.clone(),
                tickets: None,
            }),
            None => Box::new(Sink { db, model }),
        }
    }

    /// `sink`, without waiting for durability: the request's answer waits instead (`tickets`).
    pub(crate) fn sink_then<'m>(
        &self,
        db: Database,
        model: &'m quadra_ir::Model,
    ) -> Box<dyn quadra_runtime::CommitSink + 'm> {
        match &self.sequencer {
            Some(s) => Box::new(quadra_store::SequencedSink {
                db,
                model,
                sequencer: s.clone(),
                tickets: Some(self.tickets.clone()),
            }),
            None => Box::new(Sink { db, model }),
        }
    }

    /// The database was written past the sequencer (a deploy, a migration): it reads it again.
    pub(crate) fn sequencer_reload(&self) {
        if let Some(s) = &self.sequencer {
            s.reload();
        }
    }

    pub fn run(mut self, rx: mpsc::Receiver<Job>, usage: &crate::Meter) {
        use quadra_runtime::limits;
        // opening (migrations, the cache filled) is the host's work, not a request's
        limits::take_usage();
        for job in rx {
            if let Job::Stop { reply } = job {
                self.close();
                drop(self);
                let _ = reply.send(());
                return;
            }
            let long = match &job {
                Job::Sweep { .. } | Job::DevReload { .. } | Job::DevRetry { .. } => true,
                Job::Call { req, .. } => req.is_long(),
                _ => false,
            };
            let limit = if long {
                self.limits.background
            } else {
                self.limits.job
            };
            let _limits = limits::begin(limits::Limits {
                deadline: limit.map(|d| std::time::Instant::now() + d),
                max_rows: self.limits.max_rows,
                max_list: self.limits.max_list,
                max_users: self.limits.max_users,
            });
            let request = matches!(job, Job::Call { .. });
            // a request's or a sweep's work is the tenant's; the rest (sign-in, the stream of
            // commits) is the host's bookkeeping
            let counted = request || matches!(job, Job::Sweep { .. });
            let started = std::time::Instant::now();
            // a bug met by one request: it fails, the engine goes on serving the others
            if let Err(panic) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.job(job)))
            {
                eprintln!("engine: a request failed with an internal error ({}): caches and connections are reset", panic_message(&*panic));
                self.reset();
            }
            let used = limits::take_usage();
            if counted {
                usage.add(used, request, started.elapsed());
            }
        }
    }

    /// Before the engine goes away: SQLite's log written back into the database file, so that
    /// the file alone holds every commit (a host may copy it once the engine has stopped).
    fn close(&mut self) {
        self.pool.borrow_mut().clear();
        if self.conn.dialect() == quadra_store::conn::Dialect::Sqlite {
            if let Err(e) = self.conn.batch("PRAGMA wal_checkpoint(TRUNCATE)") {
                eprintln!("engine: closing the database: {e}");
            }
        }
    }

    /// Whether something of the model changes with time alone (it needs the periodic sweep).
    pub fn reads_clock(&self) -> bool {
        self.checked
            .model
            .as_ref()
            .is_some_and(quadra_analysis::reads_clock)
    }

    /// After an internal error: what a request may have left half done is thrown away (the
    /// record cache, connections with a transaction open). The model does not change.
    fn reset(&mut self) {
        // the commits handed over by the request that failed are not the next one's
        self.tickets.borrow_mut().clear();
        self.cache = Rc::new(RefCell::new(Cache::new(
            self.cache_mb << 20,
            std::time::Duration::from_secs(600),
        )));
        self.pool.borrow_mut().clear();
        match Conn::open(&self.url) {
            Ok(c) => self.conn = c,
            Err(e) => eprintln!("engine: cannot reconnect: {e}"),
        }
        self.renames = None;
        self.units = None;
    }

    fn job(&mut self, job: Job) {
        {
            match job {
                Job::Auth { secret, reply } => {
                    let p = auth::authenticate(&mut self.conn, &secret, now()).unwrap_or(None);
                    let _ = reply.send(p);
                }
                Job::DevLogin { login, reply } => {
                    let _ = reply.send(self.dev_login(&login));
                }
                Job::CodeStart { login, reply } => {
                    let _ = reply.send(self.code_start(&login));
                }
                Job::CodeVerify { id, code, reply } => {
                    let _ = reply.send(self.code_verify(&id, &code));
                }
                Job::Invite { who, user, reply } => {
                    let _ = reply.send(self.invite(&who, user));
                }
                Job::People { reply } => {
                    let _ = reply.send(self.people());
                }
                Job::AddUser {
                    email,
                    roles,
                    reply,
                } => {
                    let _ = reply.send(self.add_user(&email, &roles));
                }
                Job::Logout { secret } => {
                    let _ = self.side(move |c| auth::revoke_secret(c, &secret));
                }
                Job::Call { who, req, reply } => {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.call(&who, req)
                    })) {
                        Ok(r) => {
                            let tickets = std::mem::take(&mut *self.tickets.borrow_mut());
                            match tickets.last() {
                                // answered once the commit is durable; the engine goes on
                                Some(t) if r.status < 300 => t.then(move |res| {
                                    let _ = reply.send(match res {
                                        Ok(_) => r,
                                        Err(e) => committed_late(e),
                                    });
                                }),
                                _ => {
                                    let _ = reply.send(r);
                                }
                            }
                        }
                        Err(panic) => {
                            let _ = reply.send(error(
                                500,
                                "internal error: the request could not be completed",
                            ));
                            std::panic::resume_unwind(panic);
                        }
                    }
                }
                Job::Commits { since, reply } => {
                    let _ = reply.send(self.commits(since).unwrap_or((since, Vec::new())));
                }
                Job::DevReload { reply } => {
                    let _ = reply.send(self.dev_reload());
                }
                Job::DevRetry { reply } => {
                    let data = self.problems.as_ref().is_some_and(|p| p["kind"] == "data");
                    let ev = if data {
                        // the files are the same: forget them, so that they are applied again
                        self.seen.clear();
                        self.dev_reload()
                    } else {
                        None
                    };
                    let _ = reply.send(ev);
                }
                Job::Stop { .. } => unreachable!("handled by `run`"),
                Job::NextSweep { reply } => {
                    let _ = reply.send(self.next_sweep());
                }
                Job::Deliveries { limit, reply } => {
                    let _ = reply.send(self.deliveries(limit));
                }
                Job::Attach { limit, reply } => {
                    let _ = reply.send(self.attach(limit));
                }
                Job::Files { reply } => {
                    let _ = reply.send(self.files());
                }
                Job::Receive {
                    connection,
                    path,
                    headers,
                    body,
                    reply,
                } => {
                    let _ = reply.send(self.receive(&connection, &path, &headers, &body));
                }
                Job::Endpoint {
                    connection,
                    method,
                    path,
                    query,
                    headers,
                    body,
                    reply,
                } => {
                    let _ = reply.send(self.endpoint(
                        &connection,
                        &method,
                        &path,
                        &query,
                        &headers,
                        &body,
                    ));
                }
                Job::Pulls { reply } => {
                    let _ = reply.send(self.pulls());
                }
                Job::AgentHello {
                    code,
                    version,
                    reply,
                } => {
                    let _ = reply.send(self.agent_hello(&code, &version));
                }
                Job::AgentKey {
                    key,
                    version,
                    reply,
                } => {
                    let _ = reply.send(self.agent_key(&key, &version));
                }
                Job::Pulled {
                    inbound,
                    result,
                    reply,
                } => {
                    self.save_consents();
                    let _ = reply.send(self.pulled(&inbound, result));
                }
                Job::Consent { step, reply } => {
                    let _ = reply.send(self.consent_step(step));
                }
                Job::Inbox { limit, reply } => {
                    let _ = reply.send(self.handle_inbox(limit));
                }
                Job::Defer { id, at, reply } => {
                    let at = at.format("%Y-%m-%dT%H:%M:%S").to_string();
                    let _ =
                        reply.send(self.side(move |c| quadra_store::deliveries::defer(c, id, &at)));
                }
                Job::Delivered {
                    id,
                    event,
                    outcome,
                    attempt,
                    fields,
                    attempts,
                    answer,
                    reply,
                } => {
                    self.save_consents();
                    let at = now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let (ev, o, a) = (event.clone(), outcome.clone(), attempt.clone());
                    let r = self
                        .side(move |c| quadra_store::deliveries::record(c, id, &ev, &at, &o, &a));
                    // a final outcome, written now: the reactions to it, in a transaction
                    if let Ok(true) = r {
                        self.outcome_reactions(
                            id,
                            &event,
                            &outcome,
                            &attempt,
                            &fields,
                            attempts + 1,
                            answer.as_deref(),
                        );
                    }
                    let _ = reply.send(r.map(|_| ()));
                }
                Job::Sweep { reply } => {
                    let checked = self.checked.clone();
                    // only thresholds read the clock and none passed: no need to read every record
                    // (`quadra_analysis::clock`); at least one sweep a day all the same
                    if reply.is_none()
                        && self
                            .next_sweep()
                            .is_ok_and(|at| at.is_some_and(|at| at > now()))
                    {
                        return;
                    }
                    let r = quadra_store::ops::sweep_with(
                        &self.url,
                        checked.model.as_ref().unwrap(),
                        &checked.report,
                        now(),
                        self.sequencer.as_ref(),
                    );
                    if let Err(e) = &r {
                        eprintln!("sweep: {e}");
                    }
                    self.archive_due();
                    self.cold_due();
                    if let Some(reply) = reply {
                        let _ = reply.send(r);
                    }
                }
            }
        }
    }

    /// When the next sweep is needed: `Err(())` periodically (the clock is read in a way only a
    /// periodic sweep follows), `Ok(None)` never, `Ok(Some(at))` at the nearest threshold not
    /// passed at the last sweep, a day after the last sweep at the latest.
    fn next_sweep(&mut self) -> Result<Option<chrono::NaiveDateTime>, ()> {
        let checked = self.checked.clone();
        let (Some(model), report) = (checked.model.as_ref(), &checked.report) else {
            return Err(());
        };
        let n = now();
        match quadra_store::ops::next_due(&self.url, model, report, n) {
            Ok(quadra_store::ops::Due::Never) => Ok(None),
            Ok(quadra_store::ops::Due::At(at)) => {
                let last = quadra_store::ops::last_sweep(&self.url, model, report)
                    .ok()
                    .flatten();
                let net = last.map_or(n, |l| l + chrono::Duration::hours(24));
                Ok(Some(at.map_or(net, |a| a.min(net))))
            }
            Ok(quadra_store::ops::Due::Periodic) | Err(_) => Err(()),
        }
    }

    /// A request of the API. A limit of the job that was hit (limits.rs) is the answer, even
    /// where it only left a value missing on the way.
    fn call(&mut self, who: &Principal, req: Req) -> Reply {
        let who = who.clone();
        self.snapshot = req.reads();
        let r = match req {
            Req::Me => self.me(&who),
            Req::Model => self.model(&who),
            Req::Query(q) => self.query(&who, &q),
            Req::Record { entity, id, base } => self.record(&who, &entity, id, base),
            Req::Export { entity, id } => self.export(&who, &entity, id),
            Req::Trash { entity } => self.trash(&who, entity.as_deref()),
            Req::History {
                entity,
                id,
                limit,
                before,
            } => self.history(&who, &entity, id, limit, before),
            Req::File {
                entity,
                id,
                field,
                n,
                download,
            } => self.file(&who, &entity, id, &field, n, download),
            Req::Upload { body } => self.upload(&who, &body),
            Req::Uploads { body, key } => self.uploads_due(&who, &body, &key),
            Req::UploadsChecked { body } => self.uploads_checked(&who, &body),
            Req::Search {
                entity,
                q,
                limit,
                after,
            } => self.search(&who, &entity, &q, limit, after.as_deref()),
            Req::Tx { body, key } => self.tx(&who, &body, &key),
            Req::DevFiles => self.dev_files(&who),
            Req::DevRead { path } => self.dev_read(&who, &path),
            Req::DevSave { path, text } => self.dev_save(&who, &path, &text),
            Req::DevDelete { path } => self.dev_delete(&who, &path),
            Req::DevCheck { buffers } => self.dev_check(&who, &buffers),
            Req::DevTest { buffers } => self.dev_test(&who, &buffers),
            Req::DevStatus => self.dev_status(&who),
            Req::DevDiff => self.dev_diff(&who),
            Req::DevCommit { message } => self.dev_commit(&who, &message),
            Req::DevIde { body } => self.dev_ide(&who, &body),
            Req::DevRename { from, to } => self.dev_rename(&who, &from, &to),
            Req::DevMkdir { path } => self.dev_mkdir(&who, &path),
            Req::DevRmdir { path } => self.dev_rmdir(&who, &path),
            Req::DevSchema { body } => self.dev_schema(&who, &body),
            Req::DevLive => Ok(self::reply(
                200,
                json!({ "watching": self.watch, "problems": self.problems }),
            )),
            Req::Views {
                method,
                path,
                query,
                body,
            } => self.views_api(&who, &method, &path, &query, &body),
            Req::Outbox { path, query, body } => self.outbox_api(&who, &path, &query, &body),
            #[cfg(test)]
            Req::Panic => panic!("a test of the recovery"),
        };
        self.snapshot = false;
        let r = r.unwrap_or_else(|(s, m)| error(s, m));
        match quadra_runtime::limits::take_hit() {
            Some(m) => error(422, m),
            None => r,
        }
    }

    /// An interpreter over the database as of `base` (default: the latest commit).
    fn interp<'m>(
        &mut self,
        model: &'m Model,
        who: Option<&Principal>,
        base: Option<u64>,
    ) -> Result<Interp<'m>, Fail> {
        let e500 = |e: String| (500, e);
        let mut conn = Pooled::take(&self.pool, &self.url).map_err(e500)?;
        // a read sees one moment, even while other engines commit: its SQL queries can then be
        // answered by SQL (no row newer than what the interpreter reads)
        if self.snapshot {
            conn.begin_read().map_err(e500)?;
        }
        // the caches catching up with commits: bookkeeping, neither limited nor counted
        let (base, latest) = quadra_runtime::limits::uncounted(|| -> Result<(u64, u64), Fail> {
            let latest = head(&mut conn).map_err(e500)?;
            let base = base.unwrap_or(latest);
            if base > latest {
                return Err((
                    400,
                    format!("base {base} is ahead of the latest commit ({latest})"),
                ));
            }
            let renamed = quadra_runtime::limits::without_row_cap(|| {
                self.cache.borrow_mut().sync(&mut conn, base)
            })
            .map_err(e500)?;
            if renamed || self.renames.is_none() {
                self.renames = Some(load_renames(&mut conn).map_err(e500)?);
                self.units = None;
            }
            // units, currencies and the company: kept while none of those records is written
            if let (true, Some((_, _, at))) = (base == latest, &self.units) {
                let changed =
                    conn.query("SELECT '1' FROM \"_quadra_records\" WHERE \"last_tx\" > ?bigint AND \"entity\" IN ('Uom', 'Currency', 'Company') LIMIT 1", &[quadra_store::conn::Sql::Int(*at as i64)]).map_err(e500)?;
                if !changed.is_empty() {
                    self.units = None;
                }
            }
            Ok((base, latest))
        })?;
        let current = base == latest;
        let renames = self.renames.clone().unwrap_or_default();
        let mut src = DbSource::with_renames(
            conn,
            base,
            self.schema.clone(),
            Some(self.cache.clone()),
            renames,
        )
        .map_err(e500)?;
        // ids come from the sequencer: no write of the engine's own
        src.sequencer = self.sequencer.clone();
        let mut i = Interp::new(model);
        i.statics = self.statics_of(model);
        i.set_clock(now());
        i.store = Store::over(Rc::new(RefCell::new(src)));
        match (&self.units, current) {
            (Some((u, company, _)), true) => {
                i.set_units(u);
                i.company = *company;
            }
            _ => {
                i.load_units();
                i.company = i.ids("Company", None).first().copied();
                if current {
                    self.units = Some((i.units(), i.company, latest));
                }
            }
        }
        i.user = who.map(|w| w.user_id);
        Ok(i)
    }

    fn login_of(&mut self, model: &Model, user: u64) -> String {
        match self.interp(model, None, None) {
            Ok(mut i) => i
                .store
                .read(user, "login", None)
                .map(|v| v.to_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    fn dev_login(&mut self, login: &str) -> Reply {
        if !self.dev_login {
            return error(404, "not found");
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = match self.interp(model, None, None) {
            Ok(i) => i,
            Err((s, m)) => return error(s, m),
        };
        let users = i.ids("User", None);
        // `quadra dev`: signed in without asking, as the first user (`dev`)
        let auto = login.is_empty() && self.watch;
        let Some(user) = users.into_iter().find(|id| {
            auto || i
                .store
                .read(*id, "login", None)
                .map_or(false, |v| v.to_string() == login)
        }) else {
            return error(401, "unknown user");
        };
        drop(i);
        let name = format!("dev login {login}");
        match self.side(move |c| {
            auth::create(
                c,
                "session",
                user,
                &name,
                false,
                now(),
                Some(now() + chrono::Duration::hours(12)),
            )
        }) {
            Ok((secret, csrf)) => Reply {
                status: 200,
                body: json!({ "user": if auto { self.login_of(model, user) } else { login.to_string() }, "csrf": csrf }),
                session: Some(secret),
            },
            Err(e) => error(500, e),
        }
    }

    /// A user by login or by email, and where their email goes: the `email` field if the
    /// project's User has one and it is set, else the login when it is an address.
    fn user_by(&mut self, model: &Model, key: &str) -> Option<(u64, String, Option<String>)> {
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            return None;
        }
        let has_email = model
            .entity("User")
            .is_some_and(|e| e.fields.iter().any(|f| f.name == "email"));
        let mut i = self.interp(model, None, None).ok()?;
        for id in i.ids("User", None) {
            let login = i
                .store
                .read(id, "login", None)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let email = if has_email {
                i.store
                    .read(id, "email", None)
                    .filter(|v| !v.is_none())
                    .map(|v| v.to_string())
            } else {
                None
            };
            if login.to_lowercase() == key
                || email.as_deref().is_some_and(|e| e.to_lowercase() == key)
            {
                let to = email.or_else(|| login.contains('@').then(|| login.clone()));
                return Some((id, login, to));
            }
        }
        None
    }

    /// The roles a user can have here and the users (at most 1000), for the host.
    fn people(&mut self) -> Reply {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let roles: Vec<&str> = model.roles.iter().map(|r| r.name.as_str()).collect();
        let has_email = model
            .entity("User")
            .is_some_and(|e| e.fields.iter().any(|f| f.name == "email"));
        let mut i = match self.interp(model, None, None) {
            Ok(i) => i,
            Err((s, m)) => return error(s, m),
        };
        let ids = i.ids("User", None);
        let total = ids.len();
        let users: Vec<J> = ids
            .into_iter()
            .take(1000)
            .map(|id| {
                let login = i
                    .store
                    .read(id, "login", None)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let email = if has_email {
                    i.store
                        .read(id, "email", None)
                        .filter(|v| !v.is_none())
                        .map(|v| v.to_string())
                } else {
                    None
                };
                let roles: Vec<String> = match i.store.read(id, "roles", None) {
                    Some(V::Set(items)) => items
                        .iter()
                        .map(|r| match r {
                            V::Role(n) => n.to_string(),
                            other => other.to_string(),
                        })
                        .collect(),
                    _ => vec![],
                };
                json!({ "id": id, "login": login, "email": email, "roles": roles })
            })
            .collect();
        reply(
            200,
            json!({ "roles": roles, "users": users, "total": total }),
        )
    }

    /// The host adds a user with these roles, their login the address, and asks to invite them.
    /// A user who has that login or email already is invited again, unchanged.
    fn add_user(&mut self, email: &str, roles: &[String]) -> Reply {
        let email = email.trim().to_lowercase();
        let valid = email.len() <= 254
            && !email.contains(char::is_whitespace)
            && email.split_once('@').is_some_and(|(a, d)| {
                !a.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
            });
        if !valid {
            return error(400, "Not an email address.");
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        if let Some(r) = roles
            .iter()
            .find(|r| !model.roles.iter().any(|m| &m.name == *r))
        {
            return error(400, format!("This project has no role `{r}`."));
        }
        if let Some((id, login, to)) = self.user_by(model, &email) {
            let send = to.map(|to| json!({ "to": to, "invite": true, "login": login }));
            return reply(
                200,
                json!({ "id": id, "login": login, "created": false, "_send": send }),
            );
        }
        let mut i = match self.interp(model, None, None) {
            Ok(i) => i,
            Err((s, m)) => return error(s, m),
        };
        let conn = match Pooled::take(&self.pool, &self.url) {
            Ok(c) => c,
            Err(e) => return error(500, e),
        };
        let db = Database::with(conn, self.schema.clone());
        i.materialize = materialized_formulas(&db.schema);
        i.sink = Some(self.sink(db, model));
        i.system += 1;
        let set: Vec<V> = roles
            .iter()
            .map(|r| V::Role(Rc::from(r.as_str())))
            .collect();
        let created = RefCell::new(0u64);
        let r = i.with_cause(format!("added by the host: user {email}"), |i| {
            i.transaction(|i| {
                if let V::Ref { id, .. } = i.create(
                    "User",
                    vec![
                        ("login".to_string(), V::Text(Rc::from(email.as_str()))),
                        ("roles".to_string(), V::Set(Rc::new(set))),
                    ],
                )? {
                    *created.borrow_mut() = id;
                }
                Ok(())
            })
        });
        match r {
            Ok(()) => reply(
                200,
                json!({ "id": created.into_inner(), "login": email, "created": true, "_send": { "to": email, "invite": true, "login": email } }),
            ),
            Err(e) => error(422, describe(e)),
        }
    }

    /// The first step of the sign-in by email. The answer does not say whether the user exists:
    /// without one (or without an address) there is a request id and nothing is sent.
    fn code_start(&mut self, login: &str) -> Reply {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let Some((user, _, Some(to))) = self.user_by(model, login) else {
            return Reply {
                status: 200,
                body: json!({ "login_id": auth::random_hex(16), "expires_in": auth::CODE_MINUTES * 60 }),
                session: None,
            };
        };
        match self.side(move |c| auth::code_start(c, user, now())) {
            Ok(Ok((id, code))) => Reply {
                status: 200,
                body: json!({ "login_id": id, "expires_in": auth::CODE_MINUTES * 60, "_send": { "to": to, "code": code } }),
                session: None,
            },
            // too many codes for this user: the same answer as for an unknown login (no email),
            // so that the answer does not tell which logins exist; the last code still works
            Ok(Err(_)) => Reply {
                status: 200,
                body: json!({ "login_id": auth::random_hex(16), "expires_in": auth::CODE_MINUTES * 60 }),
                session: None,
            },
            Err(e) => error(500, e),
        }
    }

    fn code_verify(&mut self, id: &str, code: &str) -> Reply {
        let (id, code) = (id.to_string(), code.to_string());
        match self.side(move |c| auth::code_verify(c, &id, &code, now())) {
            Ok(Ok(user)) => {
                let checked = self.checked.clone();
                let model = checked.model.as_ref().unwrap();
                let login = self.login_of(model, user);
                let name = format!("email code {login}");
                match self.side(move |c| {
                    auth::create(
                        c,
                        "session",
                        user,
                        &name,
                        false,
                        now(),
                        Some(now() + chrono::Duration::days(SESSION_DAYS)),
                    )
                }) {
                    Ok((secret, csrf)) => Reply {
                        status: 200,
                        body: json!({ "user": login, "csrf": csrf }),
                        session: Some(secret),
                    },
                    Err(e) => error(500, e),
                }
            }
            Ok(Err((why, left))) => {
                let msg = match why {
                    "wrong" => format!(
                        "Wrong code. {left} attempt{} left.",
                        if left == 1 { "" } else { "s" }
                    ),
                    "locked" => "Too many wrong attempts with this code. Ask for a new one.".into(),
                    "used" => "This code was used already. Ask for a new one.".into(),
                    "expired" => {
                        "This code has expired, or a newer one was sent. Ask for a new one.".into()
                    }
                    _ => "This sign-in request is not known. Ask for a new code.".into(),
                };
                Reply {
                    status: 400,
                    body: json!({ "error": msg, "code": why }),
                    session: None,
                }
            }
            Err(e) => error(500, e),
        }
    }

    /// An admin invites a user: their address, for the host to write to.
    fn invite(&mut self, who: &Principal, user: u64) -> Reply {
        let admin = match self.who(who) {
            Ok(w) => w.admin,
            Err((s, m)) => return error(s, m),
        };
        if !admin {
            return error(403, "Only an admin can invite users.");
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let login = self.login_of(model, user);
        if login.is_empty() {
            return error(404, "No such user.");
        }
        match self.user_by(model, &login) {
            Some((_, login, Some(to))) => Reply {
                status: 200,
                body: json!({ "invited": login, "_send": { "to": to, "invite": true, "login": login } }),
                session: None,
            },
            _ => error(
                400,
                "This user has no email address: give them an `email`, or a login that is one.",
            ),
        }
    }

    fn me(&mut self, who: &Principal) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let login = self.login_of(model, who.user_id);
        Ok(reply(
            200,
            json!({ "user": login, "user_id": who.user_id, "read_only": who.read_only, "csrf": who.csrf, "kind": who.kind }),
        ))
    }

    // ---- model --------------------------------------------------------------------------

    /// Entities, fields, actions and flows the user may see, with what they may do.
    fn model(&mut self, who: &Principal) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        let modules = &self.modules;
        let entities = quadra_api::model_json(&mut i, model, &checked.report, &|s| {
            modules.get(&s.file).cloned().unwrap_or_default()
        });
        Ok(reply(
            200,
            json!({ "entities": entities, "base": i.store.base }),
        ))
    }

    // ---- expressions written by users ---------------------------------------------------

    /// The model with formulas added to an entity (`where` filters, personal columns),
    /// checked like any source: the same messages as in the editor.
    fn overlay(&mut self, entity: &str, items: &[(String, String)]) -> Result<Rc<Checked>, Reply> {
        if items.is_empty() {
            return Ok(self.checked.clone());
        }
        let key = format!(
            "{entity}\u{1}{}",
            items
                .iter()
                .map(|(n, e)| format!("{n}\u{2}{e}"))
                .collect::<Vec<_>>()
                .join("\u{1}")
        );
        if let Some(c) = self.overlays.get(&key) {
            return Ok(c.clone());
        }
        let src = quadra_api::overlay_source(entity, items);
        let mut sources = self.files.clone();
        let path = self.root.join("tests/__api__.qd");
        sources.push((path.clone(), src));
        let project = Project::from_sources(&self.root, sources);
        let checked = with_env(quadra_analysis::check(&project), &self.env);
        let errors: Vec<String> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        if !errors.is_empty() || checked.model.is_none() {
            return Err(reply(
                400,
                json!({ "error": "the expression does not check", "diagnostics": errors }),
            ));
        }
        // the only difference allowed: these formulas on this entity
        let (base, over) = (
            self.checked.model.as_ref().unwrap(),
            checked.model.as_ref().unwrap(),
        );
        let same_shape = quadra_api::same_shape(base, over, entity, items);
        if !same_shape {
            return Err(error(400, "an expression must be a single value"));
        }
        let c = Rc::new(checked);
        self.overlays.insert(key.clone(), c.clone());
        self.overlay_order.push_back(key);
        if self.overlay_order.len() > 64 {
            if let Some(old) = self.overlay_order.pop_front() {
                self.overlays.remove(&old);
            }
        }
        Ok(c)
    }

    // ---- reading ------------------------------------------------------------------------

    fn query(&mut self, who: &Principal, q: &J) -> Result<Reply, Fail> {
        let (entity, items, columns) = quadra_api::query_items(q)?;
        if self
            .checked
            .model
            .as_ref()
            .unwrap()
            .entity(&entity)
            .map_or(true, |e| e.kind != EntityKind::Entity || e.temporary)
        {
            return Err((404, format!("unknown entity `{entity}`")));
        }
        let checked = match self.overlay(&entity, &items) {
            Ok(c) => c,
            Err(r) => return Ok(r),
        };
        let model = checked.model.as_ref().unwrap();
        let base = q["base"].as_u64();
        let mut i = self.interp(model, Some(who), base)?;
        // expressions of the caller (columns, `where`): evaluated as that user, as an app's
        // cells are: rows they may read, hidden fields and records refused
        if !columns.is_empty() || !q["where"].is_null() {
            i.viewer = true;
        }
        Ok(reply(
            200,
            quadra_api::query_rows(&mut i, model, &entity, q, &items, &columns)?,
        ))
    }

    fn record(
        &mut self,
        who: &Principal,
        entity: &str,
        id: u64,
        base: Option<u64>,
    ) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), base)?;
        match quadra_api::record_json(&mut i, model, entity, id) {
            Ok(j) => Ok(reply(200, j)),
            Err((404, m)) => Err(self.erased(id).unwrap_or((404, m))),
            Err(e) => Err(e),
        }
    }

    /// A record taken away by its retention: `410` with when and why.
    /// `[data] archive_after = "2 years"`: once a day, the history older than that moved to an
    /// archive in the store of files, with its checkpoint, through the sequencer.
    /// Once a day: the closed records of the entities with `archive …` moved to a new volume,
    /// written to the store of files first, through the sequencer (crate quadra_store::cold).
    fn cold_due(&mut self) {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return;
        };
        if !model
            .entities
            .iter()
            .any(|e| matches!(e.cold, Some(quadra_ir::Cold::After { .. })))
        {
            return;
        }
        if self
            .cold_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(24 * 3600))
        {
            return;
        }
        self.cold_at = Some(std::time::Instant::now());
        let files = match self.files() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cold: {e}");
                return;
            }
        };
        let mut put = |key: &str, bytes: &[u8]| {
            files
                .blobs
                .put(key, bytes, "application/vnd.sqlite3", "attachment")
                .map_err(|e| e.to_string())
        };
        match quadra_store::ops::cold_run(
            &self.url,
            model,
            &checked.report,
            now(),
            self.sequencer.as_ref(),
            &mut put,
            &mut |key: &str| files.blobs.delete(key).map_err(|e| e.to_string()),
        ) {
            Ok(Some((v, n))) => eprintln!("cold: {n} closed records moved to the volume {v}"),
            Ok(None) => {}
            Err(e) => eprintln!("cold: {e}"),
        }
    }

    /// The volumes of the cold records not beside the database (a database rebuilt from its
    /// log, another machine): brought from the store of files before anything reads.
    pub(crate) fn fetch_cold(&self) -> Result<(), String> {
        let files = match self.files() {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        quadra_store::cold::fetch(&self.url, &mut |k| {
            files.blobs.get(k).map_err(|e| e.to_string())
        })
        .map(|_| ())
    }

    fn archive_due(&mut self) {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return;
        };
        let Some((n, unit)) = model.manifest.data.archive_after.clone() else {
            return;
        };
        if self
            .archived_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(24 * 3600))
        {
            return;
        }
        self.archived_at = Some(std::time::Instant::now());
        let term = quadra_ir::Term {
            n: n as i64,
            unit,
            after: String::new(),
            span: quadra_ir::Span {
                file: quadra_sema::FileId(0),
                range: Default::default(),
            },
        };
        let at = now();
        let Some(before) = term.before(at) else {
            return;
        };
        let files = match self.files() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("archive: {e}");
                return;
            }
        };
        let mut put = |key: &str, bytes: &[u8]| {
            files
                .blobs
                .put(key, bytes, "application/gzip", "attachment")
                .map_err(|e| e.to_string())
        };
        match quadra_store::archive::archive_with(
            &self.url,
            model,
            &checked.report,
            before,
            at,
            self.sequencer.as_ref(),
            &mut put,
        ) {
            Ok(Some(c)) => eprintln!(
                "archive: commit {}: {} values moved to {}",
                c.tx, c.rows, c.archive
            ),
            Ok(None) => {}
            Err(e) => eprintln!("archive: {e}"),
        }
    }

    /// The messages received and the events whose term ended (`keep …`, `[data] inbox`,
    /// `outbox`): first written to an archive in the store of files when they say `then
    /// archive` (not pruned if it cannot be written), then emptied or deleted.
    fn prune_messages(&mut self, model: &quadra_ir::Model) {
        use quadra_store::deliveries::{message_terms, prune_messages, to_archive};
        let (mut inbox, mut outbox) = message_terms(model, now());
        if inbox.iter().chain(&outbox).any(|k| k.archive) {
            let written = to_archive(&mut self.conn, &inbox, &outbox, 100_000).and_then(|rows| {
                if rows.is_empty() {
                    return Ok(());
                }
                let lines: Vec<J> = rows
                    .into_iter()
                    .map(|(kind, mut r)| {
                        r["kind"] = J::String(kind);
                        r
                    })
                    .collect();
                let bytes = quadra_store::archive::gz_lines(&lines)?;
                let key = format!(
                    "archive/messages-{}.jsonl.gz",
                    now().format("%Y%m%dT%H%M%S")
                );
                self.files()?
                    .blobs
                    .put(&key, &bytes, "application/gzip", "attachment")
                    .map_err(|e| e.to_string())
            });
            if let Err(e) = written {
                eprintln!(
                    "integrations: the messages to archive were not archived, they stay: {e}"
                );
                inbox.retain(|k| !k.archive);
                outbox.retain(|k| !k.archive);
            }
        }
        match self.side(move |c| prune_messages(c, &inbox, &outbox)) {
            Ok((0, 0)) => {}
            Ok((emptied, deleted)) => eprintln!(
                "integrations: {emptied} messages emptied, {deleted} messages and events deleted (their term ended)"
            ),
            Err(e) => eprintln!("integrations: the old messages were not pruned: {e}"),
        }
    }

    fn erased(&mut self, id: u64) -> Option<Fail> {
        // in the trash: it can come back
        if let Ok(r) = self.conn.query(
            "SELECT \"entity\", \"deleted_at\", \"until_at\" FROM \"_quadra_trash\" WHERE \"id\" = ?bigint",
            &[quadra_store::conn::Sql::Int(id as i64)],
        ) {
            if let Some(r) = r.into_iter().next() {
                let ent = r[0].clone().unwrap_or_default();
                return Some((
                    410,
                    format!(
                        "{ent}#{id} is in the trash since {} (until {}): `POST /v1/records/{ent}/{id}/restore` gives it back",
                        r[1].clone().unwrap_or_default().replace('T', " "),
                        r[2].clone().unwrap_or_default().replace('T', " "),
                    ),
                ));
            }
        }
        let r = self
            .conn
            .query(
                "SELECT \"entity\", \"erased_at\", \"cause\" FROM \"_quadra_erased\" WHERE \"id\" = ?bigint",
                &[quadra_store::conn::Sql::Int(id as i64)],
            )
            .ok()?;
        let r = r.into_iter().next()?;
        let cause = r[2].clone().unwrap_or_default();
        let verb = if cause.starts_with("delete:") || cause.starts_with("trash:") {
            "deleted"
        } else {
            "erased"
        };
        Some((
            410,
            format!(
                "{}#{id} was {verb} on {}: {}",
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default().replace('T', " "),
                r[2].clone().unwrap_or_default()
            ),
        ))
    }

    /// `GET /v1/trash[?entity=E]`: the records in the trash of the entities the caller may
    /// delete, the latest first, with their values (those the caller may see) and until when.
    fn trash(&mut self, who: &Principal, entity: Option<&str>) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        let rows = self
            .conn
            .query(
                "SELECT CAST(\"id\" AS TEXT), \"entity\", \"data\", \"deleted_at\", CAST(\"user_id\" AS TEXT), \"until_at\" FROM \"_quadra_trash\" ORDER BY \"_quadra_trash\".\"deleted_at\" DESC LIMIT 1000",
                &[],
            )
            .map_err(|e| (500, e))?;
        let mut allowed: HashMap<String, bool> = HashMap::new();
        let mut out = Vec::new();
        for r in rows {
            let ent = r[1].clone().unwrap_or_default();
            if entity.is_some_and(|e| e != ent) {
                continue;
            }
            let ok = *allowed
                .entry(ent.clone())
                .or_insert_with(|| i.check_permission("delete", &ent, None, None).is_ok());
            if !ok {
                continue;
            }
            let data: J = serde_json::from_str(r[2].as_deref().unwrap_or("{}")).unwrap_or(J::Null);
            let mut values = serde_json::Map::new();
            if let Some(o) = data["records"][0]["values"].as_object() {
                for (f, v) in o {
                    if !i.hidden(&ent, f) {
                        values.insert(f.clone(), v.clone());
                    }
                }
            }
            out.push(serde_json::json!({
                "id": r[0].as_deref().and_then(|v| v.parse::<u64>().ok()),
                "entity": ent,
                "values": values,
                "lines": data["records"].as_array().map_or(0, |a| a.len().saturating_sub(1)),
                "deleted_at": r[3],
                "deleted_by": r[4].as_deref().and_then(|v| v.parse::<u64>().ok()),
                "until": r[5],
            }));
        }
        Ok(reply(200, serde_json::json!({ "records": out })))
    }

    fn export(&mut self, who: &Principal, entity: &str, id: u64) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        let out = match quadra_api::subject_json(&mut i, model, entity, id) {
            Ok(j) => j,
            Err((404, m)) => return Err(self.erased(id).unwrap_or((404, m))),
            Err(e) => return Err(e),
        };
        // the export is in the chain: who exported whom, and when
        let conn = Pooled::take(&self.pool, &self.url).map_err(|e| (500, e))?;
        let db = Database::with(conn, self.schema.clone());
        i.materialize = materialized_formulas(&db.schema);
        i.sink = Some(self.sink_then(db, model));
        i.with_cause(format!("export: {entity}#{id}"), |i| {
            i.transaction(|_| Ok(()))
        })
        .map_err(|e| (500, describe(e)))?;
        Ok(reply(200, out))
    }

    fn search(
        &mut self,
        who: &Principal,
        entity: &str,
        q: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        Ok(reply(
            200,
            quadra_api::search_rows(&mut i, model, entity, q, limit, after)?,
        ))
    }

    /// The history of a record, newest first in pages (`limit`, `before`: the `seq` of the
    /// oldest entry of the previous page), or all of it in order.
    fn history(
        &mut self,
        who: &Principal,
        entity: &str,
        id: u64,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        let actual = i
            .store
            .get(id)
            .map(|r| r.ent.to_string())
            .ok_or((404, format!("{entity}#{id} not found")))?;
        let rec = V::Ref {
            ent: Rc::from(actual.as_str()),
            id,
        };
        if !i.can("read", &actual, None, Some(&rec)) || !i.can_read_history(&actual) {
            return Err((
                403,
                format!("permission denied: cannot read the history of {actual}#{id}"),
            ));
        }
        let renames = quadra_store::source::load_renames(&mut self.conn).map_err(|e| (500, e))?;
        let rows = self
            .conn
            .query(
                &format!(
                    "SELECT CAST(h.\"tx\" AS TEXT), h.\"field\", h.\"value\", c.\"at\", CAST(c.\"user_id\" AS TEXT), c.\"cause\", CAST(h.\"seq\" AS TEXT) FROM \"_quadra_history\" h JOIN \"_quadra_commits\" c ON c.\"tx\" = h.\"tx\" WHERE h.\"record_id\" = ?bigint{} ORDER BY h.\"seq\"{}",
                    if before.is_some() { " AND h.\"seq\" < ?bigint" } else { "" },
                    match limit {
                        Some(n) => format!(" DESC LIMIT {}", n.clamp(1, 500) + 1),
                        None => String::new(),
                    }
                ),
                &[Some(quadra_store::conn::Sql::Int(id as i64)), before.map(|b| quadra_store::conn::Sql::Int(b as i64))].into_iter().flatten().collect::<Vec<_>>(),
            )
            .map_err(|e| (500, e))?;
        // a page (newest first): one more row says whether there are older entries
        let more = limit.map_or(false, |n| rows.len() > n.clamp(1, 500));
        let mut rows = rows;
        if more {
            rows.pop();
        }
        let oldest: Option<u64> = rows
            .last()
            .and_then(|r| r[6].as_deref())
            .and_then(|s| s.parse().ok());
        if limit.is_some() {
            rows.reverse();
        }
        // the oldest values may be in the archives of the history (`quadra db archive`): read
        // back with the last page, before the others
        let first_archived = rows.len();
        if !more {
            let mut old = self.archived_history(id);
            old.extend(rows);
            rows = old;
        }
        let archived_count = rows.len() - first_archived;
        let mut logins: HashMap<u64, String> = HashMap::new();
        let mut out = Vec::new();
        for r in rows {
            let tx: u64 = r[0].as_deref().and_then(|t| t.parse().ok()).unwrap_or(0);
            let field = renamed(&renames, &actual, tx, r[1].clone().unwrap_or_default());
            let base = field.split(['@', '#']).next().unwrap_or("").to_string();
            if i.hidden(&actual, &base) {
                continue;
            }
            let v = quadra_store::codec::from_json(
                &serde_json::from_str(r[2].as_deref().unwrap_or("null")).unwrap_or(J::Null),
            );
            let user = match r[4].as_deref().and_then(|u| u.parse::<u64>().ok()) {
                Some(u) => Some(
                    logins
                        .entry(u)
                        .or_insert_with(|| {
                            i.store
                                .read(u, "login", None)
                                .map(|v| v.to_string())
                                .unwrap_or_default()
                        })
                        .clone(),
                ),
                None => None,
            };
            let mut entry = json!({ "tx": tx, "at": r[3], "user": user, "cause": r[5], "field": field, "value": cell(&mut i, &v) });
            if out.len() < archived_count {
                entry["archived"] = json!(true);
            }
            out.push(entry);
        }
        Ok(reply(
            200,
            json!({ "entity": actual, "id": id, "entries": out, "before": if more { oldest } else { None } }),
        ))
    }

    /// The entries of a record's history in the archives, oldest first, as the history query
    /// gives them (tx, field, value, at, user, cause, seq). None when there are no archives, or
    /// they cannot be read (said once in the log).
    fn archived_history(&mut self, id: u64) -> Vec<Vec<Option<String>>> {
        let Ok(checkpoints) = quadra_store::archive::checkpoints(&mut self.conn) else {
            return Vec::new();
        };
        if checkpoints.is_empty() {
            return Vec::new();
        }
        let Ok(files) = self.files() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for c in &checkpoints {
            let read = files
                .blobs
                .get(&c.archive)
                .and_then(|b| b.ok_or("not in the store of files".into()))
                .and_then(|b| quadra_store::archive::read(c, &b));
            match read {
                Ok(r) => rows.extend(r.into_iter().filter(|r| r.2 == id as i64)),
                Err(e) => eprintln!("archives: {}: {e}", c.archive),
            }
        }
        rows.sort_by_key(|r| r.0);
        let mut out = Vec::new();
        for (seq, tx, _, field, value, _, _) in rows {
            let c = self
                .conn
                .query(
                    "SELECT \"at\", CAST(\"user_id\" AS TEXT), \"cause\" FROM \"_quadra_commits\" WHERE \"tx\" = ?bigint",
                    &[quadra_store::conn::Sql::Int(tx)],
                )
                .ok()
                .and_then(|r| r.into_iter().next())
                .unwrap_or_else(|| vec![None, None, None]);
            out.push(vec![
                Some(tx.to_string()),
                Some(field),
                value,
                c[0].clone(),
                c[1].clone(),
                c[2].clone(),
                Some(seq.to_string()),
            ]);
        }
        out
    }

    // ---- writing --------------------------------------------------------------------------

    fn tx(&mut self, who: &Principal, body: &J, key: &str) -> Result<Reply, Fail> {
        if who.read_only {
            return Err((403, "this token is read-only".into()));
        }
        if key.is_empty()
            || key.len() > 100
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c))
        {
            return Err((
                400,
                "Idempotency-Key: 1 to 100 characters among letters, digits, - _ : .".into(),
            ));
        }
        let hash = auth::sha256_hex(&body.to_string());
        if let Some((h, resp, tx)) =
            auth::idempotent(&mut self.conn, who.user_id, key).map_err(|e| (500, e))?
        {
            if h != hash {
                return Err((
                    422,
                    "this Idempotency-Key was already used for a different request".into(),
                ));
            }
            // the first request's commit is in the database: confirmed again only once it is
            // durable (under a strict rule, in the destinations too)
            if let Some(s) = &self.sequencer {
                if s.durable().map_or(true, |d| d < tx) {
                    return Err((503, "the first request with this Idempotency-Key is not kept yet: send it again in a moment".into()));
                }
            }
            let mut r: J = serde_json::from_str(&resp).unwrap_or(json!({}));
            r["tx"] = json!(tx);
            r["replayed"] = json!(true);
            return Ok(reply(200, r));
        }
        let ops = body["ops"]
            .as_array()
            .ok_or((400, "`ops` is required".to_string()))?;
        if ops.is_empty() || ops.len() > 200 {
            return Err((400, "1 to 200 operations per transaction".into()));
        }
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        // the uploads it names, by the hash of their files (engine/uploads.rs)
        let (resolved, uploads) = self.resolve_uploads(who, model, ops)?;
        let ops = &resolved;
        let base = body["base"].as_u64();
        // (an erasure decides nothing on values read: it needs no base)
        if base.is_none()
            && ops.iter().any(|o| {
                // (`erase`, `delete` and `restore` take a record as it is now)
                !["create", "erase", "delete", "restore"]
                    .iter()
                    .any(|k| o["op"] == json!(k))
            })
        {
            return Err((400, "`base` is required to change existing records: the commit is refused if they changed since".into()));
        }
        // The client's base guards what the client decided on: the records it changes. If others
        // changed them since it read them, its change would overwrite theirs unseen: refused.
        // Everything else (totals, rollups, the records the rules read) is read now, and the
        // rules are checked on it now: a credit limit, a stock level hold on the latest data.
        if let Some(b) = base {
            let latest = head(&mut self.conn).map_err(|e| (500, e))?;
            if b > latest {
                return Err((
                    400,
                    format!("base {b} is ahead of the latest commit ({latest})"),
                ));
            }
        }
        let targets: Vec<u64> = ops
            .iter()
            .filter(|o| o["op"] != json!("create"))
            .filter_map(|o| o["id"].as_u64())
            .collect();
        // a commit of another engine may land while this one runs: then it runs again on it
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut i = self.interp(model, Some(who), None)?;
            // checked on the data this attempt runs on: a commit that lands later changes what
            // it read (the records it changes among them), and it runs again, checked again
            if let Some(b) = base {
                let conflicts = self.changed_since(model, &targets, b)?;
                if !conflicts.is_empty() {
                    return Ok(reply(
                        409,
                        json!({ "error": "changed by others since you read it: review their changes and try again", "conflicts": conflicts }),
                    ));
                }
            }
            let conn = Pooled::take(&self.pool, &self.url).map_err(|e| (500, e))?;
            let db = Database::with(conn, self.schema.clone());
            i.materialize = materialized_formulas(&db.schema);
            i.sink = Some(self.sink_then(db, model));
            let created: RefCell<serde_json::Map<String, J>> = RefCell::new(serde_json::Map::new());
            let user = who.user_id;
            let r = i.with_cause(format!("API request {key}"), |i| {
                i.transaction(|i| {
                    *created.borrow_mut() = quadra_api::apply_ops(i, model, ops)?;
                    let response =
                        json!({ "created": J::Object(created.borrow().clone()) }).to_string();
                    i.store.idempotency = Some((user, key.to_string(), hash.clone(), response));
                    Ok(())
                })
            });
            return match r {
                Ok(()) => {
                    let tx = i.store.tx;
                    drop(i);
                    if !uploads.is_empty() {
                        let used = uploads.clone();
                        if let Err(e) =
                            self.side(move |c| quadra_store::files::uploads_used(c, &used))
                        {
                            eprintln!("uploads: not marked used: {e}");
                        }
                    }
                    Ok(reply(
                        200,
                        json!({ "tx": tx, "base": tx, "created": J::Object(created.into_inner()) }),
                    ))
                }
                Err(Flow::Conflict(_)) if attempt < 5 => continue,
                Err(Flow::Conflict(c)) => {
                    let list: Vec<J> = c.iter().map(|x| json!({ "tx": x.tx, "at": x.at, "user": x.user, "cause": x.cause, "what": x.what })).collect();
                    Ok(reply(
                        409,
                        json!({ "error": "changed by others while it was being saved, several times: try again", "conflicts": list }),
                    ))
                }
                Err(Flow::Fail(m)) if m.contains("_quadra_idempotency") => {
                    // the same key committed by another process at the same time
                    drop(i);
                    self.tx(who, body, key)
                }
                Err(Flow::Fail(m))
                    if m.starts_with("permission denied") || m.contains(": permission denied") =>
                {
                    Err((403, m))
                }
                Err(other) => Err((422, describe(other))),
            };
        }
    }

    /// What others wrote on these records after `base`: one entry per commit, with the stored
    /// fields it changed (computed values follow from other changes, they are not decisions).
    fn changed_since(&mut self, model: &Model, records: &[u64], base: u64) -> Result<Vec<J>, Fail> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        // the sequencer remembers the recent writes: nothing written since is the usual answer
        if let Some(s) = &self.sequencer {
            if s.written_since(base, records.to_vec()) == Some(false) {
                return Ok(Vec::new());
            }
        }
        let ids = records
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .conn
            .query(
                &format!("SELECT CAST(h.\"tx\" AS TEXT), CAST(h.\"record_id\" AS TEXT), h.\"field\", c.\"at\", CAST(c.\"user_id\" AS TEXT), c.\"cause\" FROM \"_quadra_history\" h JOIN \"_quadra_commits\" c ON c.\"tx\" = h.\"tx\" WHERE h.\"record_id\" IN ({ids}) AND h.\"tx\" > ?bigint ORDER BY h.\"seq\" LIMIT 500"),
                &[quadra_store::conn::Sql::Int(base as i64)],
            )
            .map_err(|e| (500, e))?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut i = self.interp(model, None, None)?;
        let mut commits: std::collections::BTreeMap<u64, (J, Option<String>, J, Vec<String>)> =
            std::collections::BTreeMap::new();
        let num = |s: &Option<String>| s.as_deref().and_then(|v| v.parse::<u64>().ok());
        for r in rows {
            let (Some(tx), Some(id)) = (num(&r[0]), num(&r[1])) else {
                continue;
            };
            let field = r[2].clone().unwrap_or_default();
            let Some(ent) = i.store.get(id).map(|x| x.ent.to_string()) else {
                continue;
            };
            let name = field.split(['@', '#']).next().unwrap_or("");
            let computed = model
                .entity(&ent)
                .and_then(|e| model.find_field(e, name))
                .map_or(false, |f| f.formula.is_some());
            if computed {
                continue;
            }
            let user = num(&r[4]).map(|u| {
                i.store
                    .read(u, "login", None)
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            });
            let e = commits
                .entry(tx)
                .or_insert_with(|| (json!(r[3]), user, json!(r[5]), Vec::new()));
            let what = format!("{ent}#{id}.{field}");
            if !e.3.contains(&what) {
                e.3.push(what);
            }
        }
        Ok(commits.into_iter().map(|(tx, (at, user, cause, what))| json!({ "tx": tx, "at": at, "user": user, "cause": cause, "what": what })).collect())
    }

    // ---- live updates ---------------------------------------------------------------------

    /// Commits after `since`: (latest transaction, events).
    fn commits(&mut self, since: u64) -> Result<(u64, Vec<J>), String> {
        use quadra_store::conn::Sql;
        let mut latest = head(&mut self.conn)?;
        // only what is confirmed: a commit still on its way to durability is not announced
        if let Some(s) = &self.sequencer {
            latest = latest.min(s.durable()?);
        }
        if latest <= since {
            return Ok((latest, Vec::new()));
        }
        let rows = self.conn.query(
            "SELECT CAST(\"tx\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT), \"cause\" FROM \"_quadra_commits\" WHERE \"tx\" > ?bigint AND \"tx\" <= ?bigint ORDER BY \"_quadra_commits\".\"tx\" LIMIT 100",
            &[Sql::Int(since as i64), Sql::Int(latest as i64)],
        )?;
        let mut out = Vec::new();
        let mut last = since;
        for r in rows {
            let tx: u64 = r[0].as_deref().and_then(|t| t.parse().ok()).unwrap_or(0);
            last = tx;
            let recs = self.conn.query(
                "SELECT DISTINCT CAST(h.\"record_id\" AS TEXT), r.\"entity\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE h.\"tx\" = ?bigint LIMIT 500",
                &[Sql::Int(tx as i64)],
            )?;
            let records: Vec<J> = recs.iter().map(|x| json!({ "entity": x[1], "id": x[0].as_deref().and_then(|i| i.parse::<u64>().ok()) })).collect();
            out.push(json!({ "tx": tx, "at": r[1], "user_id": r[2].as_deref().and_then(|u| u.parse::<u64>().ok()), "cause": r[3], "records": records }));
        }
        Ok((last, out))
    }
}

/// The limits of the jobs, through the engine's own loop: a request that runs too long,
/// reads too much or meets a bug fails alone, and the next one is served.
#[cfg(test)]
mod limit_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const PROJECT: &str = "entity Item:\n    n: Int\n\nfn burn(n: Int) -> Int:\n    if n == 0:\n        return 1\n    return burn(n - 1) + burn(n - 1)\n\nfn deep(n: Int) -> Int:\n    if n == 0:\n        return 0\n    return deep(n - 1)\n";

    struct Running {
        jobs: mpsc::Sender<Job>,
        dir: PathBuf,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn who() -> Principal {
        Principal {
            user_id: 1,
            read_only: false,
            kind: "token".into(),
            csrf: None,
            name: "test".into(),
        }
    }

    /// An engine with `items` records of `Item`, running its loop on a thread of its own.
    fn start(name: &str, items: usize, limits: Limits) -> Running {
        let dir = std::env::temp_dir().join(format!("quadra-limits-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"limits\"\n").unwrap();
        std::fs::write(dir.join("main.qd"), PROJECT).unwrap();
        let url = format!("sqlite:{}", dir.join("data.db").display());
        let project = Project::load(&dir).unwrap();
        let checked = quadra_analysis::check(&project);
        let errors: Vec<_> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        quadra_store::ops::init(
            &url,
            checked.model.as_ref().unwrap(),
            &checked.report,
            now(),
        )
        .unwrap();
        let cfg = Config {
            outside: Default::default(),
            root: dir.clone(),
            url,
            port: 0,
            dev_login: false,
            secure_cookies: false,
            cache_mb: 16,
            prewarm_days: None,
            web: None,
            sweep_every: 0,
            editors: vec![],
            watch: false,
            limits: Limits::none(),
            engines: Engines::one(),
        };
        // the records first, without limits
        let (jobs, rx) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let run_cfg = Config {
            limits,
            ..cfg.clone()
        };
        std::thread::Builder::new().stack_size(ENGINE_STACK).spawn(move || {
            let mut setup = Engine::new(&cfg).unwrap();
            for n in 0..items {
                let r = setup.call(&who(), Req::Tx { body: json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": n } }] }), key: format!("item-{n}") });
                assert_eq!(r.status, 200, "{}", r.body);
            }
            drop(setup);
            let engine = Engine::new(&run_cfg).unwrap();
            ready_tx.send(()).unwrap();
            engine.run(rx, &crate::Meter::default());
        }).unwrap();
        ready.recv().unwrap();
        Running { jobs, dir }
    }

    fn call(r: &Running, req: Req) -> Reply {
        let (tx, rx) = oneshot::channel();
        r.jobs
            .send(Job::Call {
                who: who(),
                req,
                reply: tx,
            })
            .unwrap();
        rx.blocking_recv().expect("an answer")
    }

    fn items(columns: J) -> Req {
        Req::Query(json!({ "entity": "Item", "fields": ["n"], "columns": columns }))
    }

    #[test]
    fn a_request_past_its_time_limit_fails_and_the_next_one_is_served() {
        let limit = Duration::from_millis(400);
        let r = start(
            "timeout",
            1,
            Limits {
                job: Some(limit),
                ..Limits::none()
            },
        );
        let t = Instant::now();
        let a = call(&r, items(json!({ "x": "burn(190)" })));
        let took = t.elapsed();
        assert_eq!(a.status, 422, "{}", a.body);
        assert!(
            a.body["error"].as_str().unwrap().starts_with("timeout"),
            "{}",
            a.body
        );
        assert!(took < limit + limit / 10, "answered after {took:?}");
        let b = call(&r, items(json!({ "x": "burn(3)" })));
        assert_eq!(b.status, 200, "{}", b.body);
        assert_eq!(b.body["rows"][0]["cells"]["x"]["d"], json!("8"));
    }

    #[test]
    fn a_function_that_calls_itself_too_deep_fails_without_ending_the_process() {
        let r = start("deep", 1, Limits::default());
        let a = call(&r, items(json!({ "x": "deep(100000)" })));
        assert_eq!(a.status, 200, "{}", a.body);
        assert!(
            a.body["rows"][0]["cells"]["x"]
                .to_string()
                .contains("recursion too deep"),
            "{}",
            a.body
        );
        let b = call(&r, items(json!({ "x": "deep(150)" })));
        assert_eq!(
            b.body["rows"][0]["cells"]["x"]["d"],
            json!("0"),
            "{}",
            b.body
        );
    }

    #[test]
    fn a_bug_fails_one_request_and_the_engine_goes_on() {
        let r = start("panic", 2, Limits::default());
        let a = call(&r, Req::Panic);
        assert_eq!(a.status, 500, "{}", a.body);
        let b = call(&r, items(json!({})));
        assert_eq!(b.status, 200, "{}", b.body);
        assert_eq!(b.body["rows"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_request_that_reads_too_many_rows_fails() {
        let r = start(
            "rows",
            20,
            Limits {
                max_rows: Some(10),
                ..Limits::none()
            },
        );
        let a = call(&r, items(json!({})));
        assert_eq!(a.status, 422, "{}", a.body);
        assert!(
            a.body["error"]
                .as_str()
                .unwrap()
                .starts_with("too many rows"),
            "{}",
            a.body
        );
        // a request that reads few rows is served
        let b = call(
            &r,
            Req::Query(json!({ "entity": "Item", "fields": ["n"], "where": "n < 3" })),
        );
        assert_eq!(b.status, 200, "{}", b.body);
    }

    #[test]
    fn a_list_beyond_the_cap_fails_while_it_grows() {
        let r = start(
            "list",
            20,
            Limits {
                max_list: Some(100),
                ..Limits::none()
            },
        );
        let a = call(
            &r,
            Req::Query(
                json!({ "entity": "Item", "fields": ["n"], "where": "n == 0", "columns": { "x": "Item.flat_map(a => Item.map(b => b.n)).count()" } }),
            ),
        );
        assert_eq!(a.status, 422, "{}", a.body);
        assert!(
            a.body["error"]
                .as_str()
                .unwrap()
                .starts_with("too many elements"),
            "{}",
            a.body
        );
        let b = call(
            &r,
            Req::Query(
                json!({ "entity": "Item", "fields": ["n"], "where": "n == 0", "columns": { "x": "Item.map(b => b.n).count()" } }),
            ),
        );
        assert_eq!(b.status, 200, "{}", b.body);
        assert_eq!(b.body["rows"][0]["cells"]["x"]["d"], json!("20"));
    }
}
