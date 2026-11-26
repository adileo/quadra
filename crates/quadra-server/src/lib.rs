//! HTTP API for Quadra.
//!
//! - Authentication on every request: `Authorization: Bearer <token>` for programs, an
//!   `HttpOnly; SameSite=Strict` session cookie for the web client, with a CSRF header on
//!   every write and an `Origin` check.
//! - `POST /v1/tx` needs an `Idempotency-Key`: a retry returns the first response, the
//!   same key with a different body is refused. It also needs the `base` transaction the
//!   client read: if what the transaction relies on changed since, the answer is 409 with
//!   who changed what, and nothing is written.
//! - Filters and personal columns are Quadra expressions, type checked like source code
//!   and evaluated by the engine: never SQL.
//! - Limits: request size, rows per query, operations per transaction, requests per second.

mod engine;

pub use engine::{app_params, bind_for, open_requests, unhex, AppRequest, Config, Engines, Limits, Local, ENGINE_STACK};
use engine::{Engine, Job, Reply, Req};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use quadra_store::auth::Principal;
use serde_json::{json, Value as J};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};

/// The tenants a host serves, found by the address a request was sent to.
///
/// `quadra serve` serves one project at every address. A host of many projects (Quadra
/// Cloud) gives its own: the configuration of the project behind each address, which it
/// prepared beforehand (its code in a folder, its database at a url).
pub trait TenantResolver: Send + Sync + 'static {
    /// The tenant for the `Host` of a request (without the port), if any.
    fn resolve(&self, host: &str) -> Option<TenantSpec>;
}

/// A tenant: its id (the same id is the same running engine) and the configuration of its
/// engine. `port` and `web` of the configuration are the host's, not the tenant's.
#[derive(Clone)]
pub struct TenantSpec {
    pub id: String,
    pub config: Config,
}

/// One project at every address: `quadra serve`.
struct Single(TenantSpec);

impl TenantResolver for Single {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

/// A running tenant: its engine's queue of jobs and its live events.
/// One engine of a tenant: its queue of jobs.
struct Slot {
    jobs: Mutex<mpsc::Sender<Job>>,
    /// jobs sent to it and not answered yet
    pending: Arc<std::sync::atomic::AtomicUsize>,
    /// its engine has started (a follower that failed to start never is)
    ready: Arc<std::sync::atomic::AtomicBool>,
}

impl Slot {
    fn load(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A running tenant: its engines (the first writer, then more writers and readers) and its
/// live events.
struct Tenant {
    id: String,
    /// the engines' configuration
    config: Config,
    /// `writers[0]` is the first engine; more only on PostgreSQL
    writers: Mutex<Vec<Arc<Slot>>>,
    readers: Mutex<Vec<Arc<Slot>>>,
    /// held while engines are added or all removed: a stop never misses one being added
    lifecycle: Mutex<()>,
    events: broadcast::Sender<J>,
    last_used: Mutex<Instant>,
    /// why its first engine could not start (then a later request starts it again)
    failed: Arc<Mutex<Option<String>>>,
    /// stopped by the host: its background tasks end at once (`true`)
    stop: tokio::sync::watch::Sender<bool>,
    /// the sweeps of rules that read the clock: every `sweep_every` seconds, if it has any
    sweep_every: Option<Duration>,
    reads_clock: Arc<std::sync::atomic::AtomicBool>,
    last_sweep: Mutex<Instant>,
    /// jobs sent to its engines and not answered yet, and how many may wait
    pending: Arc<std::sync::atomic::AtomicUsize>,
    max_queue: Option<usize>,
    usage: Arc<Meter>,
    /// the one place its commits are ordered and written (`quadra_store::sequencer`): started
    /// by its first engine, used by all of them
    sequencer: Arc<Mutex<Option<quadra_store::Sequencer>>>,
    durability: Option<Arc<dyn Durability>>,
}

/// What a tenant's engines used, added after each job and taken by the host
/// (`Host::take_usage`).
#[derive(Default)]
pub(crate) struct Meter {
    requests: std::sync::atomic::AtomicU64,
    steps: std::sync::atomic::AtomicU64,
    rows_read: std::sync::atomic::AtomicU64,
    rows_written: std::sync::atomic::AtomicU64,
    engine_us: std::sync::atomic::AtomicU64,
}

impl Meter {
    pub(crate) fn add(&self, u: quadra_runtime::limits::Usage, request: bool, took: Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        self.requests.fetch_add(request as u64, Relaxed);
        self.steps.fetch_add(u.steps, Relaxed);
        self.rows_read.fetch_add(u.rows_read, Relaxed);
        self.rows_written.fetch_add(u.rows_written, Relaxed);
        self.engine_us.fetch_add(took.as_micros() as u64, Relaxed);
    }

    fn take(&self) -> Usage {
        use std::sync::atomic::Ordering::Relaxed;
        Usage {
            requests: self.requests.swap(0, Relaxed),
            steps: self.steps.swap(0, Relaxed),
            rows_read: self.rows_read.swap(0, Relaxed),
            rows_written: self.rows_written.swap(0, Relaxed),
            engine_ms: self.engine_us.swap(0, Relaxed) / 1000,
        }
    }
}

/// What a tenant used since the host last asked: its API requests and their compute. Steps
/// and rows do not depend on the machine or its load (the same request on the same data
/// counts the same); `engine_ms` is the time its engines worked, for the host's own checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub requests: u64,
    /// expressions evaluated
    pub steps: u64,
    pub rows_read: u64,
    pub rows_written: u64,
    pub engine_ms: u64,
}

impl Usage {
    fn plus(self, o: Usage) -> Usage {
        Usage { requests: self.requests + o.requests, steps: self.steps + o.steps, rows_read: self.rows_read + o.rows_read, rows_written: self.rows_written + o.rows_written, engine_ms: self.engine_ms + o.engine_ms }
    }

    pub fn is_zero(&self) -> bool {
        *self == Usage::default()
    }
}

/// One job waiting for an engine: counted while it waits.
struct Waiting(Arc<std::sync::atomic::AtomicUsize>);

impl Waiting {
    fn new(n: &Arc<std::sync::atomic::AtomicUsize>) -> Waiting {
        n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Waiting(n.clone())
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The least busy of the engines that are ready.
fn least_busy(slots: &[Arc<Slot>]) -> Option<Arc<Slot>> {
    slots.iter().filter(|s| s.is_ready()).min_by_key(|s| s.load()).cloned()
}

impl Tenant {
    /// The engine for a job: a read to the least busy reader (the writers only when no reader
    /// is ready), anything else to the least busy writer.
    fn route(&self, job: &Job) -> Option<Arc<Slot>> {
        if job.reads() {
            if let Some(r) = least_busy(&self.readers.lock().unwrap()) {
                return Some(r);
            }
        }
        // writes of data run on any engine once the sequencer orders the commits
        if job.writes_data() && self.sequencer.lock().unwrap().is_some() {
            let all: Vec<Arc<Slot>> = self.readers.lock().unwrap().iter().chain(self.writers.lock().unwrap().iter()).cloned().collect();
            if let Some(s) = least_busy(&all) {
                return Some(s);
            }
        }
        let writers = self.writers.lock().unwrap();
        // the first writer takes jobs even while it starts: they wait in its queue (none:
        // the tenant was stopped)
        least_busy(&writers).or_else(|| writers.first().cloned())
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Job) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        let job = make(tx);
        let slot = self.route(&job)?;
        let _all = Waiting::new(&self.pending);
        let _one = Waiting::new(&slot.pending);
        slot.jobs.lock().ok()?.send(job).ok()?;
        rx.await.ok()
    }

    /// A job without an answer.
    fn send(&self, job: Job) {
        if let Some(slot) = self.route(&job) {
            let _ = slot.jobs.lock().map(|j| j.send(job));
        }
    }

    /// Too many requests already wait for the engines.
    fn full(&self) -> bool {
        self.max_queue.is_some_and(|max| self.pending.load(std::sync::atomic::Ordering::Relaxed) >= max)
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn halt(&self) {
        self.stop.send_replace(true);
    }

    /// Waits `d`, or less if the tenant is stopped meanwhile: false then.
    async fn pause(&self, d: Duration) -> bool {
        let mut stop = self.stop.subscribe();
        if *stop.borrow_and_update() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => !self.stopped(),
            _ = stop.changed() => false,
        }
    }

    /// How many engines it may have: one when the files are watched (`quadra dev` applies
    /// them to its engine).
    fn shape(&self) -> Engines {
        if self.config.watch {
            return Engines::one();
        }
        // the sequencer orders the commits: any number of engines may prepare them, on SQLite too
        Engines { readers: self.config.engines.readers, writers: self.config.engines.writers.max(1) }
    }

    /// The engines besides the first, started (again) now: after the first one is up, and
    /// after the code changed under them (a deploy from the editor). The old ones finish what
    /// they have and stop.
    fn start_followers(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap();
        if self.stopped() {
            return;
        }
        let shape = self.shape();
        let new_readers: Vec<Arc<Slot>> = (0..shape.readers).map(|n| engine_thread(&self.id, &format!("r{n}"), &self.config, false, None, None, self.usage.clone(), self.sequencer.clone(), self.durability.clone())).collect();
        let new_writers: Vec<Arc<Slot>> = (1..shape.writers).map(|n| engine_thread(&self.id, &format!("w{n}"), &self.config, false, None, None, self.usage.clone(), self.sequencer.clone(), self.durability.clone())).collect();
        let old_readers = std::mem::replace(&mut *self.readers.lock().unwrap(), new_readers);
        let old_writers = {
            let mut w = self.writers.lock().unwrap();
            let rest: Vec<Arc<Slot>> = w.drain(1..).collect();
            w.extend(new_writers);
            rest
        };
        for old in old_readers.into_iter().chain(old_writers) {
            let (tx, _) = oneshot::channel();
            let _ = old.jobs.lock().map(|j| j.send(Job::Stop { reply: tx }));
        }
    }

    /// Every engine: they answer what they have, close the database and stop.
    async fn stop_engines(&self) {
        // (the tenant is halted: no engine is added after this)
        let slots: Vec<Arc<Slot>> = {
            let _lifecycle = self.lifecycle.lock().unwrap();
            self.readers.lock().unwrap().drain(..).chain(self.writers.lock().unwrap().drain(..)).collect()
        };
        for s in slots {
            let (tx, rx) = oneshot::channel();
            if s.jobs.lock().map(|j| j.send(Job::Stop { reply: tx })).is_ok_and(|r| r.is_ok()) {
                let _ = rx.await;
            }
        }
        // what the sequencer holds is written before the database is handed back
        let sequencer = self.sequencer.lock().unwrap().take();
        if let Some(s) = sequencer {
            let _ = tokio::task::spawn_blocking(move || s.stop()).await;
        }
    }

    /// Its engines that are serving.
    fn engines(&self) -> usize {
        self.readers.lock().unwrap().iter().chain(self.writers.lock().unwrap().iter()).filter(|s| s.is_ready()).count()
    }
}

/// An engine on a thread of its own (it owns state that is not thread-safe). The first engine
/// of a tenant (`first`) brings the database up to date with the project; it reports whether
/// the project reads the clock, and why it could not start.
#[allow(clippy::too_many_arguments)]
fn engine_thread(
    id: &str,
    name: &str,
    cfg: &Config,
    first: bool,
    failed: Option<Arc<Mutex<Option<String>>>>,
    reads_clock: Option<Arc<std::sync::atomic::AtomicBool>>,
    usage: Arc<Meter>,
    sequencer: Arc<Mutex<Option<quadra_store::Sequencer>>>,
    durability: Option<Arc<dyn Durability>>,
) -> Arc<Slot> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (jobs, rx) = mpsc::channel::<Job>();
    let slot = Arc::new(Slot { jobs: Mutex::new(jobs), pending: Arc::new(AtomicUsize::new(0)), ready: Arc::new(AtomicBool::new(false)) });
    let ready = slot.ready.clone();
    let (cfg, label) = (Config { web: None, ..cfg.clone() }, format!("{id}/{name}"));
    let tenant = id.to_string();
    let thread_label = label.clone();
    let spawned = std::thread::Builder::new().name(format!("quadra-engine-{label}")).stack_size(engine::ENGINE_STACK).spawn(move || {
        let label = thread_label;
        let engine = if first { Engine::new(&cfg) } else { Engine::follower(&cfg) };
        // the first engine has brought the database up to date: its sequencer starts there
        let engine = engine.and_then(|mut engine| {
            let mut slot = sequencer.lock().unwrap();
            if first || slot.is_none() {
                if let Some(old) = slot.take() {
                    old.stop();
                }
                let (destinations, rule) = durability.as_ref().map(|d| d.destinations(&tenant)).unwrap_or((Vec::new(), quadra_store::sequencer::Rule::Local));
                let options = quadra_store::sequencer::Options { max_bytes: cfg.limits.max_bytes, statement_timeout: cfg.limits.job, destinations, rule, ..Default::default() };
                *slot = Some(quadra_store::Sequencer::start(&cfg.url, options)?);
            }
            engine.set_sequencer(slot.clone());
            Ok(engine)
        });
        match engine {
            Ok(engine) => {
                if let Some(c) = &reads_clock {
                    c.store(engine.reads_clock(), Ordering::Relaxed);
                }
                ready.store(true, Ordering::Relaxed);
                engine.run(rx, &usage);
            }
            Err(e) => {
                eprintln!("{label}: the project cannot start: {e}");
                // the jobs waiting in the queue get no answer: `the engine is not available`
                if let Some(f) = &failed {
                    *f.lock().unwrap() = Some(e);
                }
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("{label}: cannot start a thread: {e}");
    }
    slot
}

/// An email of the sign-in, written by whoever hosts the server (Quadra Cloud): the text is
/// theirs, the server only says whom and what.
#[derive(Clone, Debug)]
pub enum SignInMail {
    /// the code of a sign-in
    Code { to: String, code: String },
    /// an admin invited the user of `login` to the app
    Invite { to: String, login: String },
}

/// Sends the sign-in's emails: with it, users sign in with a code sent by email
/// (`POST /v1/auth/code`, `/v1/auth/code/verify`) and admins invite them (`/v1/auth/invite`).
/// `tenant` is the tenant's id, `host` the address the request came to (`shop.quadra.run`).
/// Where a tenant's commits are kept besides its database, and when they are confirmed to the
/// client (`quadra_store::sequencer`): the host decides it per tenant (its plan).
pub trait Durability: Send + Sync + 'static {
    fn destinations(&self, tenant: &str) -> (Vec<Box<dyn quadra_store::sequencer::DurabilitySink>>, quadra_store::sequencer::Rule);
}

/// Every batch as a file in a folder of its own per tenant (`quadra serve --log-dir`): a
/// commit is confirmed once its batch is in the folder too.
pub struct LogFolder(pub std::path::PathBuf);

impl Durability for LogFolder {
    fn destinations(&self, tenant: &str) -> (Vec<Box<dyn quadra_store::sequencer::DurabilitySink>>, quadra_store::sequencer::Rule) {
        match quadra_store::segments::Folder::new(self.0.join(tenant)) {
            Ok(f) => (vec![Box::new(f)], quadra_store::sequencer::Rule::All),
            Err(e) => {
                eprintln!("log folder: {e}");
                (Vec::new(), quadra_store::sequencer::Rule::Local)
            }
        }
    }
}

pub trait SignIn: Send + Sync + 'static {
    fn send(&self, tenant: &str, host: &str, mail: SignInMail) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
}

struct App {
    resolver: Arc<dyn TenantResolver>,
    running: Mutex<HashMap<String, Arc<Tenant>>>,
    secure_cookies: bool,
    dev_login: bool,
    sign_in: Option<Arc<dyn SignIn>>,
    /// requests per second a credential may make
    rate: f64,
    durability: Option<Arc<dyn Durability>>,
    /// requests per valid credential (`tenant/hash`): (tokens, last refill)
    limiter: Mutex<HashMap<String, (f64, Instant)>>,
    /// failed authentications per tenant: (tokens, last refill)
    failures: Mutex<HashMap<String, (f64, Instant)>>,
    /// credentials found valid lately (`tenant/hash` → who, until when): a request with one
    /// does not wait for the engine, and still gets in while a flood of wrong ones is refused
    known: Mutex<HashMap<String, (Principal, Instant)>>,
    /// what tenants stopped since the host last asked had used
    used: Mutex<HashMap<String, Usage>>,
}

type Shared = Arc<App>;

/// The tenant of a request, from its `Host` header.
struct Ten(Arc<Tenant>);

impl axum::extract::FromRequestParts<Shared> for Ten {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, app: &Shared) -> Result<Self, Response> {
        let host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()).or_else(|| parts.uri.host()).unwrap_or("");
        let t = app.tenant(host)?;
        // a backlog the request would only wait in: refused now, to be tried again shortly
        if t.full() {
            let mut r = fail(503, "the server is busy with this project's requests: try again in a moment");
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return Err(r);
        }
        Ok(Ten(t))
    }
}

const COOKIE: &str = "quadra_session";
const RATE: f64 = 30.0; // requests per second, per credential
const BURST: f64 = 120.0;
/// Failed authentications per second, per tenant: past them, a credential not seen valid lately
/// is refused without asking the engine.
const FAIL_RATE: f64 = 5.0;
const FAIL_BURST: f64 = 50.0;
/// How long a credential found valid is trusted without asking the engine again (a revoked token
/// or a session signed out elsewhere works at most this long more; signing out here is at once).
const KNOWN_FOR: Duration = Duration::from_secs(10);
/// Entries a limiter or the known credentials keep before the idle ones are forgotten.
const MAX_ENTRIES: usize = 10_000;

/// Takes one token from the bucket of `key` (refilled at `rate` per second up to `burst`);
/// false when it is empty. Past `MAX_ENTRIES` keys, the full buckets (idle ones) are forgotten.
fn take(buckets: &Mutex<HashMap<String, (f64, Instant)>>, key: &str, rate: f64, burst: f64) -> bool {
    let mut m = buckets.lock().unwrap();
    let now = Instant::now();
    if m.len() >= MAX_ENTRIES && !m.contains_key(key) {
        m.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * rate < burst);
    }
    let e = m.entry(key.to_string()).or_insert((burst, now));
    e.0 = (e.0 + now.duration_since(e.1).as_secs_f64() * rate).min(burst);
    e.1 = now;
    if e.0 >= 1.0 {
        e.0 -= 1.0;
        true
    } else {
        false
    }
}

/// Whether the bucket of `key` has a token left, without taking it.
fn has_token(buckets: &Mutex<HashMap<String, (f64, Instant)>>, key: &str, rate: f64) -> bool {
    let m = buckets.lock().unwrap();
    m.get(key).map_or(true, |(tokens, at)| tokens + Instant::now().duration_since(*at).as_secs_f64() * rate >= 1.0)
}

fn json_response(status: u16, body: J) -> Response {
    let mut r = (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn fail(status: u16, msg: &str) -> Response {
    json_response(status, json!({ "error": msg }))
}

impl App {
    /// The tenant of the request's address: running, or started now (its engine compiles the
    /// project while the first requests wait in its queue).
    fn tenant(&self, host: &str) -> Result<Arc<Tenant>, Response> {
        let host = host.split(':').next().unwrap_or("");
        let Some(spec) = self.resolver.resolve(host) else {
            return Err(fail(404, "no project is served at this address"));
        };
        let mut running = self.running.lock().unwrap();
        if let Some(t) = running.get(&spec.id) {
            let failed = t.failed.lock().unwrap().clone();
            match failed {
                None => {
                    *t.last_used.lock().unwrap() = Instant::now();
                    return Ok(t.clone());
                }
                // a project that cannot start is tried again at most every 10 seconds
                Some(e) if t.last_used.lock().unwrap().elapsed() < Duration::from_secs(10) => {
                    return Err(fail(503, &format!("the project cannot start: {e}")));
                }
                Some(_) => t.halt(),
            }
        }
        let t = start(spec, self.durability.clone());
        running.insert(t.id.clone(), t.clone());
        Ok(t)
    }

    /// One request more for `key` (a sign-in attempt, a code asked): false past the rate.
    fn allow(&self, key: &str) -> bool {
        take(&self.limiter, key, RATE, BURST)
    }

    /// One request more of a valid credential: false past the host's rate.
    fn allow_credential(&self, key: &str) -> bool {
        take(&self.limiter, key, self.rate, self.rate * BURST / RATE)
    }

    /// The key of a credential of a tenant: its hash, never the secret itself.
    fn credential(t: &Tenant, secret: &str) -> String {
        format!("{}/{}", t.id, quadra_store::auth::sha256_hex(secret))
    }

    fn known(&self, key: &str) -> Option<Principal> {
        let m = self.known.lock().unwrap();
        m.get(key).filter(|(_, until)| *until > Instant::now()).map(|(p, _)| p.clone())
    }

    fn remember(&self, key: String, who: &Principal) {
        let mut m = self.known.lock().unwrap();
        let now = Instant::now();
        if m.len() >= MAX_ENTRIES {
            m.retain(|_, (_, until)| *until > now);
        }
        if m.len() < MAX_ENTRIES {
            m.insert(key, (who.clone(), now + KNOWN_FOR));
        }
    }

    fn forget(&self, key: &str) {
        self.known.lock().unwrap().remove(key);
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// The caller, or the response to send instead.
async fn authenticate(app: &App, t: &Tenant, method: &Method, headers: &HeaderMap) -> Result<Principal, Response> {
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string);
    let (secret, from_cookie) = match (bearer, cookie(headers, COOKIE)) {
        (Some(b), _) => (b, false),
        (None, Some(c)) => (c, true),
        (None, None) => return Err(fail(401, "authentication required")),
    };
    let key = App::credential(t, &secret);
    let who = match app.known(&key) {
        Some(who) => who,
        None => {
            // a flood of wrong credentials stops here instead of filling the engine's queue
            if !has_token(&app.failures, &t.id, FAIL_RATE) {
                return Err(fail(429, "too many failed sign-ins on this project: try again in a moment"));
            }
            match t.ask(|reply| Job::Auth { secret, reply }).await.flatten() {
                Some(who) => {
                    app.remember(key.clone(), &who);
                    who
                }
                None => {
                    take(&app.failures, &t.id, FAIL_RATE, FAIL_BURST);
                    return Err(fail(401, "invalid or expired credentials"));
                }
            }
        }
    };
    if !app.allow_credential(&key) {
        return Err(fail(429, "too many requests"));
    }
    if from_cookie && method != Method::GET {
        // a browser session: the CSRF token must come back in a header, and the request
        // must come from this site
        let sent = headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
        if sent.is_none() || sent != who.csrf.as_deref() {
            return Err(fail(403, "missing or wrong X-CSRF-Token"));
        }
        if let (Some(origin), Some(host)) = (headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()), headers.get(header::HOST).and_then(|v| v.to_str().ok())) {
            let o = origin.trim_start_matches("https://").trim_start_matches("http://");
            if o != host {
                return Err(fail(403, "cross-site request refused"));
            }
        }
    }
    Ok(who)
}

async fn call(t: &Tenant, who: Principal, req: Req) -> Response {
    match t.ask(|reply| Job::Call { who, req, reply }).await {
        Some(Reply { status, body, .. }) => json_response(status, body),
        None => fail(503, "the engine is not available"),
    }
}

async fn me(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::Me).await,
        Err(r) => r,
    }
}

async fn model(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::Model).await,
        Err(r) => r,
    }
}

async fn query(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::Query(body)).await,
        Err(r) => r,
    }
}

async fn record(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Path((entity, id)): Path<(String, u64)>, Query(q): Query<HashMap<String, String>>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::Record { entity, id, base: q.get("base").and_then(|b| b.parse().ok()) }).await,
        Err(r) => r,
    }
}

async fn history(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Path((entity, id)): Path<(String, u64)>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|v| v.parse().ok());
    let before = q.get("before").and_then(|v| v.parse().ok());
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::History { entity, id, limit, before }).await,
        Err(r) => r,
    }
}

async fn search(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Path(entity): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let text = q.get("q").cloned().unwrap_or_default();
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20);
    let after = q.get("after").cloned();
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::Search { entity, q: text, limit, after }).await,
        Err(r) => r,
    }
}

async fn tx(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    let who = match authenticate(&app, &t, &method, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return fail(400, "Idempotency-Key header required");
    };
    let r = call(&t, who, Req::Tx { body, key }).await;
    // `quadra dev`: the records that kept the saved code from applying may be fixed now
    if r.status().is_success() {
        if let Some(Some(ev)) = t.ask(|reply| Job::DevRetry { reply }).await {
            let _ = t.events.send(ev);
        }
    }
    r
}

// ---- the code editor ---------------------------------------------------------------------

async fn dev_files(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevFiles).await,
        Err(r) => r,
    }
}

async fn dev_status(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevStatus).await,
        Err(r) => r,
    }
}

async fn dev_diff(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevDiff).await,
        Err(r) => r,
    }
}

async fn dev_read(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevRead { path: q.get("path").cloned().unwrap_or_default() }).await,
        Err(r) => r,
    }
}

async fn dev_save(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => {
            let path = body["path"].as_str().unwrap_or("").to_string();
            let text = body["text"].as_str().unwrap_or("").to_string();
            call(&t, who, Req::DevSave { path, text }).await
        }
        Err(r) => r,
    }
}

async fn dev_delete(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevDelete { path: q.get("path").cloned().unwrap_or_default() }).await,
        Err(r) => r,
    }
}

async fn dev_check(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevCheck { buffers: body["buffers"].clone() }).await,
        Err(r) => r,
    }
}

async fn dev_test(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevTest { buffers: body["buffers"].clone() }).await,
        Err(r) => r,
    }
}

async fn dev_ide(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevIde { body }).await,
        Err(r) => r,
    }
}

async fn dev_schema(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => {
            let r = call(&t, who, Req::DevSchema { body }).await;
            // the schema changed under the other engines: they start again
            if r.status().is_success() {
                t.start_followers();
            }
            r
        }
        Err(r) => r,
    }
}

async fn dev_live(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevLive).await,
        Err(r) => r,
    }
}

async fn dev_rename(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevRename { from: body["from"].as_str().unwrap_or("").into(), to: body["to"].as_str().unwrap_or("").into() }).await,
        Err(r) => r,
    }
}

async fn dev_mkdir(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevMkdir { path: body["path"].as_str().unwrap_or("").into() }).await,
        Err(r) => r,
    }
}

async fn dev_rmdir(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    match authenticate(&app, &t, &method, &headers).await {
        Ok(who) => call(&t, who, Req::DevRmdir { path: q.get("path").cloned().unwrap_or_default() }).await,
        Err(r) => r,
    }
}

async fn dev_commit(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    let who = match authenticate(&app, &t, &method, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let message = body["message"].as_str().unwrap_or("").to_string();
    match t.ask(|reply| Job::Call { who, req: Req::DevCommit { message }, reply }).await {
        Some(Reply { status, body, .. }) => {
            if status == 200 {
                // the other engines start again on the new code; every client reloads the model
                t.start_followers();
                let _ = t.events.send(json!({ "type": "deploy", "sha": body["sha"] }));
            }
            json_response(status, body)
        }
        None => fail(503, "the engine is not available"),
    }
}

/// Saved views, teams and the directory of whom to share with (engine/views.rs). Every
/// change is announced to open clients (`views` events), who read their list again.
async fn views_api(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, uri: axum::http::Uri, Query(query): Query<HashMap<String, String>>, body: axum::body::Bytes) -> Response {
    let who = match authenticate(&app, &t, &method, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let body: J = if body.is_empty() {
        J::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(j) => j,
            Err(e) => return fail(400, &format!("the body is not JSON: {e}")),
        }
    };
    // `POST /v1/eval` reads (formulas evaluated as the caller), `POST /v1/complete` completes a
    // formula: no change to announce
    let write = method != Method::GET && uri.path() != "/v1/eval" && uri.path() != "/v1/complete";
    let req = Req::Views { method: method.to_string(), path: uri.path().to_string(), query, body };
    match t.ask(|reply| Job::Call { who, req, reply }).await {
        Some(Reply { status, body, .. }) => {
            if write && status < 300 {
                let entity = body["view"]["entity"].clone();
                let _ = t.events.send(json!({ "type": "views", "entity": entity }));
            }
            // a file (an app's spreadsheet as Excel): its bytes
            if status == 200 && body["__file"].is_object() {
                let f = &body["__file"];
                let Some(bytes) = engine::unhex(f["hex"].as_str().unwrap_or("")) else {
                    return fail(500, "a file could not be sent");
                };
                let name: String = f["name"].as_str().unwrap_or("export").chars().filter(|c| c.is_ascii_alphanumeric() || " ._-()".contains(*c)).collect();
                let mut r = Response::new(Body::from(bytes));
                let h = r.headers_mut();
                if let Ok(v) = HeaderValue::from_str(f["type"].as_str().unwrap_or("application/octet-stream")) {
                    h.insert(header::CONTENT_TYPE, v);
                }
                if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
                    h.insert(header::CONTENT_DISPOSITION, v);
                }
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                return r;
            }
            json_response(status, body)
        }
        None => fail(503, "the engine is not available"),
    }
}

async fn dev_login(State(app): State<Shared>, Ten(t): Ten, Json(body): Json<J>) -> Response {
    if !app.dev_login {
        return fail(404, "not found");
    }
    let login = body["login"].as_str().unwrap_or("").to_string();
    if !app.allow(&format!("{}/login:{login}", t.id)) {
        return fail(429, "too many requests");
    }
    match t.ask(|reply| Job::DevLogin { login, reply }).await {
        Some(Reply { status, body, session }) => {
            let mut r = json_response(status, body);
            if let Some(s) = session {
                let secure = if app.secure_cookies { "; Secure" } else { "" };
                let c = format!("{COOKIE}={s}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200{secure}");
                r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap());
            }
            r
        }
        None => fail(503, "the engine is not available"),
    }
}

/// How users sign in here: what the web client shows.
async fn auth_methods(State(app): State<Shared>, Ten(_t): Ten) -> Response {
    json_response(200, json!({ "dev_login": app.dev_login, "code": app.sign_in.is_some() }))
}

/// The email's details, taken out of the engine's answer before it goes to the browser.
fn take_send(body: &mut J) -> Option<J> {
    body.as_object_mut().and_then(|o| o.remove("_send"))
}

fn host_of(headers: &HeaderMap) -> String {
    headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

/// `POST /v1/auth/code {login}`: a code by email to the user of that login or address.
async fn code_start(State(app): State<Shared>, Ten(t): Ten, headers: HeaderMap, Json(body): Json<J>) -> Response {
    let Some(sign_in) = app.sign_in.clone() else {
        return fail(404, "not found");
    };
    let login = body["login"].as_str().unwrap_or("").trim().to_lowercase();
    if login.is_empty() || login.len() > 254 {
        return fail(400, "an email address or a login");
    }
    if !app.allow(&format!("{}/code:{login}", t.id)) || !app.allow(&format!("{}/code", t.id)) {
        return fail(429, "too many requests");
    }
    let Some(mut r) = t.ask(|reply| Job::CodeStart { login, reply }).await else {
        return fail(503, "the engine is not available");
    };
    if let Some(send) = take_send(&mut r.body) {
        let mail = SignInMail::Code { to: send["to"].as_str().unwrap_or("").into(), code: send["code"].as_str().unwrap_or("").into() };
        if let Err(e) = sign_in.send(&t.id, &host_of(&headers), mail).await {
            eprintln!("quadra: the sign-in email of {} could not be sent: {e}", t.id);
            return fail(503, "the email could not be sent: try again in a moment");
        }
    }
    json_response(r.status, r.body)
}

/// `POST /v1/auth/code/verify {login_id, code}`: the session, as a cookie of this address only.
async fn code_verify(State(app): State<Shared>, Ten(t): Ten, Json(body): Json<J>) -> Response {
    if app.sign_in.is_none() {
        return fail(404, "not found");
    }
    let id = body["login_id"].as_str().unwrap_or("").to_string();
    let code = body["code"].as_str().unwrap_or("").to_string();
    if !app.allow(&format!("{}/verify:{id}", t.id)) {
        return fail(429, "too many requests");
    }
    match t.ask(|reply| Job::CodeVerify { id, code, reply }).await {
        Some(Reply { status, body, session }) => {
            let mut r = json_response(status, body);
            if let Some(s) = session {
                let secure = if app.secure_cookies { "; Secure" } else { "" };
                let c = format!("{COOKIE}={s}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}", engine::SESSION_DAYS * 86400);
                r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap());
            }
            r
        }
        None => fail(503, "the engine is not available"),
    }
}

/// `POST /v1/auth/invite {user_id}`: an admin sends a user the invitation to sign in.
async fn invite(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Json(body): Json<J>) -> Response {
    let Some(sign_in) = app.sign_in.clone() else {
        return fail(404, "not found");
    };
    let who = match authenticate(&app, &t, &method, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Some(user) = body["user_id"].as_u64() else {
        return fail(400, "user_id");
    };
    let Some(mut r) = t.ask(|reply| Job::Invite { who, user, reply }).await else {
        return fail(503, "the engine is not available");
    };
    if let Some(send) = take_send(&mut r.body) {
        let mail = SignInMail::Invite { to: send["to"].as_str().unwrap_or("").into(), login: send["login"].as_str().unwrap_or("").into() };
        if let Err(e) = sign_in.send(&t.id, &host_of(&headers), mail).await {
            eprintln!("quadra: the invitation of {} could not be sent: {e}", t.id);
            return fail(503, "the invitation could not be sent: try again in a moment");
        }
    }
    json_response(r.status, r.body)
}

async fn logout(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap) -> Response {
    if let Err(r) = authenticate(&app, &t, &method, &headers).await {
        return r;
    }
    if let Some(secret) = cookie(&headers, COOKIE) {
        app.forget(&App::credential(&t, &secret));
        t.send(Job::Logout { secret });
    }
    let mut r = json_response(200, json!({ "ok": true }));
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_static("quadra_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"));
    r
}

/// Server-sent events: one `commit` event per transaction, restricted to the entities the
/// user may read. `?since=<tx>` replays what was missed.
async fn stream(State(app): State<Shared>, Ten(t): Ten, method: Method, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let who = match authenticate(&app, &t, &method, &headers).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let readable: HashSet<String> = match t.ask(|reply| Job::Call { who: who.clone(), req: Req::Model, reply }).await {
        Some(Reply { status: 200, body, .. }) => body["entities"].as_array().map(|a| a.iter().filter_map(|e| e["name"].as_str().map(str::to_string)).collect()).unwrap_or_default(),
        _ => return fail(503, "the engine is not available"),
    };
    let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    // subscribe before reading the backlog, so that nothing committed in between is lost;
    // a live event already replayed from the backlog is skipped (see `fresh`)
    let rx = t.events.subscribe();
    let backlog = if since > 0 { t.ask(|reply| Job::Commits { since, reply }).await.map(|(_, e)| e).unwrap_or_default() } else { Vec::new() };
    let filter = move |mut ev: J| -> Option<Event> {
        if ev["type"] == json!("deploy") {
            return Some(Event::default().event("deploy").data(ev.to_string()));
        }
        if ev["type"] == json!("problems") {
            return Some(Event::default().event("problems").data(ev.to_string()));
        }
        if ev["type"] == json!("views") {
            return Some(Event::default().event("views").data(ev.to_string()));
        }
        let recs: Vec<J> = ev["records"].as_array().cloned().unwrap_or_default().into_iter().filter(|r| r["entity"].as_str().map_or(false, |e| readable.contains(e))).collect();
        if recs.is_empty() {
            return None;
        }
        ev["records"] = J::Array(recs);
        Some(Event::default().event("commit").id(ev["tx"].to_string()).data(ev.to_string()))
    };
    let state = (backlog.into_iter(), rx, filter, since);
    let s = futures_util::stream::unfold(state, |(mut backlog, mut rx, filter, mut last)| async move {
        loop {
            let ev = match backlog.next() {
                Some(e) => e,
                None => match rx.recv().await {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                },
            };
            if !fresh(&ev, &mut last) {
                continue;
            }
            if let Some(out) = filter(ev) {
                return Some((Ok::<Event, std::convert::Infallible>(out), (backlog, rx, filter, last)));
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::default()).into_response()
}

/// A commit event not delivered yet on this stream: transactions arrive in order, so one at or
/// below the last delivered (`last`) was already replayed. Other events always pass.
fn fresh(ev: &J, last: &mut u64) -> bool {
    match ev["tx"].as_u64() {
        Some(tx) if ev["type"] != json!("deploy") && ev["type"] != json!("problems") && ev["type"] != json!("views") => {
            if tx <= *last {
                return false;
            }
            *last = tx;
            true
        }
        _ => true,
    }
}

async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    r
}

/// A tenant's engine on a thread of its own, and its background tasks: live events from new
/// commits, the periodic sweep, and with `watch` the files of the project.
fn start(spec: TenantSpec, durability: Option<Arc<dyn Durability>>) -> Arc<Tenant> {
    use std::sync::atomic::AtomicBool;
    let (events, _) = broadcast::channel::<J>(256);
    let cfg = Config { web: None, ..spec.config.clone() };
    let failed = Arc::new(Mutex::new(None));
    let reads_clock = Arc::new(AtomicBool::new(false));
    let usage = Arc::new(Meter::default());
    let sequencer = Arc::new(Mutex::new(None));
    let first = engine_thread(&spec.id, "w0", &cfg, true, Some(failed.clone()), Some(reads_clock.clone()), usage.clone(), sequencer.clone(), durability.clone());
    let t = Arc::new(Tenant {
        id: spec.id.clone(),
        config: cfg.clone(),
        writers: Mutex::new(vec![first]),
        readers: Mutex::new(Vec::new()),
        lifecycle: Mutex::new(()),
        events,
        last_used: Mutex::new(Instant::now()),
        failed,
        stop: tokio::sync::watch::channel(false).0,
        sweep_every: (cfg.sweep_every > 0).then(|| Duration::from_secs(cfg.sweep_every)),
        reads_clock,
        last_sweep: Mutex::new(Instant::now()),
        pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        max_queue: cfg.limits.max_queue,
        usage,
        sequencer,
        durability,
    });
    let (watch, root) = (cfg.watch, cfg.root.clone());

    // the other engines, once the first has brought the database up to date
    let up = t.clone();
    tokio::spawn(async move {
        if up.ask(|reply| Job::Commits { since: u64::MAX, reply }).await.is_some() && !up.stopped() {
            up.start_followers();
        }
    });

    // new commits (from this server or others) → live events
    let poll = t.clone();
    tokio::spawn(async move {
        // start from the current head: (head, no events)
        let mut last = poll.ask(|reply| Job::Commits { since: u64::MAX, reply }).await.map(|(l, _)| l).unwrap_or(0);
        while poll.pause(Duration::from_millis(500)).await {
            if let Some((l, evs)) = poll.ask(|reply| Job::Commits { since: last, reply }).await {
                last = l.max(last);
                for e in evs {
                    let _ = poll.events.send(e);
                }
            }
        }
    });
    if watch {
        // `quadra dev`: files changed on disk → checked, applied, announced
        let watcher = t.clone();
        tokio::spawn(async move {
            let mut last = signature(&root);
            while watcher.pause(Duration::from_millis(400)).await {
                let now = signature(&root);
                if now == last {
                    continue;
                }
                // wait until the editor has finished writing
                tokio::time::sleep(Duration::from_millis(150)).await;
                last = signature(&root);
                if let Some(Some(ev)) = watcher.ask(|reply| Job::DevReload { reply }).await {
                    let _ = watcher.events.send(ev);
                }
            }
        });
    }
    if let Some(every) = t.sweep_every {
        let sweeper = t.clone();
        tokio::spawn(async move {
            while sweeper.pause(every).await {
                *sweeper.last_sweep.lock().unwrap() = Instant::now();
                sweeper.send(Job::Sweep { reply: None });
            }
        });
    }
    t
}

/// What a host serves, besides its tenants.
#[derive(Clone, Default)]
pub struct HostOptions {
    /// `POST /v1/auth/dev-login`: development only
    pub dev_login: bool,
    pub secure_cookies: bool,
    /// the web client from this folder instead of the one built into the binary
    pub web: Option<std::path::PathBuf>,
    /// the sign-in by email, when the host sends the emails
    pub sign_in: Option<Arc<dyn SignIn>>,
    /// requests per second a credential may make (bursts of four times as many); None: 30
    pub requests_per_second: Option<f64>,
    /// where commits are kept besides each tenant's database (None: only there)
    pub durability: Option<Arc<dyn Durability>>,
}

/// A running tenant, as its host sees it.
#[derive(Clone, Debug)]
pub struct TenantStatus {
    pub id: String,
    /// since its last request
    pub idle: Duration,
    /// when its next sweep is due, if it has rules that read the clock: a host that puts the
    /// tenant to sleep wakes it then. `None`: nothing in it changes with time alone.
    pub next_sweep: Option<Duration>,
    /// its engines that are serving (readers and writers)
    pub engines: usize,
}

/// An HTTP server of one or many projects (tenants). Each tenant has an engine on a thread of
/// its own, started by its first request and stopped by `stop`; a tenant that loops or fails
/// does not hold up the others.
#[derive(Clone)]
pub struct Host {
    app: Shared,
    web: Option<std::path::PathBuf>,
}

impl Host {
    pub fn new(options: HostOptions, resolver: Arc<dyn TenantResolver>) -> Host {
        let app = Arc::new(App { resolver, running: Mutex::new(HashMap::new()), secure_cookies: options.secure_cookies, dev_login: options.dev_login, sign_in: options.sign_in, rate: options.requests_per_second.unwrap_or(RATE), durability: options.durability.clone(), limiter: Mutex::new(HashMap::new()), failures: Mutex::new(HashMap::new()), known: Mutex::new(HashMap::new()), used: Mutex::new(HashMap::new()) });
        Host { app, web: options.web }
    }

    /// The routes of the API and of the web client.
    pub fn router(&self) -> Router {
        let mut router = Router::new()
            .route("/v1/me", get(me))
            .route("/v1/model", get(model))
            .route("/v1/query", post(query))
            .route("/v1/records/{entity}/{id}", get(record))
            .route("/v1/records/{entity}/{id}/history", get(history))
            .route("/v1/search/{entity}", get(search))
            .route("/v1/tx", post(tx))
            .route("/v1/stream", get(stream))
            .route("/v1/views", get(views_api).post(views_api))
            .route("/v1/views/{*rest}", get(views_api).put(views_api).post(views_api).delete(views_api))
            .route("/v1/teams", get(views_api).post(views_api))
            .route("/v1/teams/{*rest}", get(views_api).put(views_api).delete(views_api))
            .route("/v1/directory", get(views_api))
            .route("/v1/eval", post(views_api))
            .route("/v1/complete", post(views_api))
            .route("/v1/apps/{*rest}", get(views_api).post(views_api))
            .route("/v1/auth", get(auth_methods))
            .route("/v1/auth/code", post(code_start))
            .route("/v1/auth/code/verify", post(code_verify))
            .route("/v1/auth/invite", post(invite))
            .route("/v1/auth/dev-login", post(dev_login))
            .route("/v1/auth/logout", post(logout))
            .route("/v1/dev/files", get(dev_files))
            .route("/v1/dev/file", get(dev_read).put(dev_save).delete(dev_delete))
            .route("/v1/dev/check", post(dev_check))
            .route("/v1/dev/test", post(dev_test))
            .route("/v1/dev/status", get(dev_status))
            .route("/v1/dev/diff", get(dev_diff))
            .route("/v1/dev/commit", post(dev_commit))
            .route("/v1/dev/ide", post(dev_ide))
            .route("/v1/dev/rename", post(dev_rename))
            .route("/v1/dev/schema", post(dev_schema))
            .route("/v1/dev/live", get(dev_live))
            .route("/v1/dev/dir", post(dev_mkdir).delete(dev_rmdir));
        if let Some(web) = &self.web {
            let index = web.join("index.html");
            router = router.fallback_service(tower_http::services::ServeDir::new(web).fallback(tower_http::services::ServeFile::new(index)));
        } else if !webapp::FILES.is_empty() {
            // the web client built into the binary
            router = router.fallback(embedded);
        }
        router.layer(DefaultBodyLimit::max(2 << 20)).layer(middleware::from_fn(security_headers)).with_state(self.app.clone())
    }

    /// Starts the tenant of an address and waits until its engine serves, or says why it
    /// cannot.
    pub async fn start(&self, host: &str) -> Result<(), String> {
        let t = self.app.tenant(host).map_err(|_| format!("no project is served at `{host}`"))?;
        match t.ask(|reply| Job::Commits { since: u64::MAX, reply }).await {
            Some(_) => Ok(()),
            None => Err(t.failed.lock().unwrap().clone().unwrap_or_else(|| "the engine stopped".into())),
        }
    }

    /// Stops a tenant: the jobs already queued are answered, then its engine closes its
    /// database (SQLite: written back into the main file, ready to be copied) and its thread
    /// ends. The next request starts it again. False if it was not running.
    pub async fn stop(&self, id: &str) -> bool {
        let Some(t) = self.app.running.lock().unwrap().remove(id) else {
            return false;
        };
        t.halt();
        t.stop_engines().await;
        let u = t.usage.take();
        if !u.is_zero() {
            let mut used = self.app.used.lock().unwrap();
            let e = used.entry(t.id.clone()).or_default();
            *e = e.plus(u);
        }
        true
    }

    /// What each tenant used since the last call (running or stopped meanwhile); tenants that
    /// used nothing are left out. A host calls it now and then and keeps the totals.
    pub fn take_usage(&self) -> Vec<(String, Usage)> {
        let mut all: HashMap<String, Usage> = std::mem::take(&mut *self.app.used.lock().unwrap());
        let running: Vec<Arc<Tenant>> = self.app.running.lock().unwrap().values().cloned().collect();
        for t in running {
            let u = t.usage.take();
            if !u.is_zero() {
                let e = all.entry(t.id.clone()).or_default();
                *e = e.plus(u);
            }
        }
        all.into_iter().collect()
    }

    /// The number of users (`User` records) of the project of an address, if it is running:
    /// it never starts a tenant (a host measuring its tenants must not wake one that is going
    /// to sleep). `None`: not running.
    pub async fn user_count(&self, host: &str) -> Result<Option<u64>, String> {
        let host = host.split(':').next().unwrap_or("");
        let Some(spec) = self.app.resolver.resolve(host) else {
            return Ok(None);
        };
        let Some(t) = self.app.running.lock().unwrap().get(&spec.id).cloned() else {
            return Ok(None);
        };
        if t.stopped() {
            return Ok(None);
        }
        let Some(r) = t.ask(|reply| Job::People { reply }).await else {
            return Ok(None);
        };
        r.body["total"].as_u64().map(Some).ok_or_else(|| r.body["error"].as_str().unwrap_or("failed").to_string())
    }


    /// Runs a pass of the rules that read the clock of the tenant of an address now, starting
    /// it if needed, and waits for it: the transaction written, if anything changed. A host
    /// that woke a sleeping tenant because its next sweep was due calls it, then may put it
    /// back to sleep.
    pub async fn sweep(&self, host: &str) -> Result<Option<u64>, String> {
        let t = self.app.tenant(host).map_err(|_| format!("no project is served at `{host}`"))?;
        let r = t.ask(|reply| Job::Sweep { reply: Some(reply) }).await;
        match r {
            Some(r) => {
                *t.last_sweep.lock().unwrap() = Instant::now();
                r
            }
            None => Err(t.failed.lock().unwrap().clone().unwrap_or_else(|| "the engine stopped".into())),
        }
    }

    /// The roles of the project of an address and its users (`{roles, users: [{id, login,
    /// email, roles}], total}`), for the host's own console. Starts it if needed.
    pub async fn people(&self, host: &str) -> Result<J, (u16, String)> {
        let t = self.app.tenant(host).map_err(|_| (404, format!("no project is served at `{host}`")))?;
        let r = t.ask(|reply| Job::People { reply }).await.ok_or((503, "the engine is not available".to_string()))?;
        if r.status != 200 {
            return Err((r.status, r.body["error"].as_str().unwrap_or("failed").to_string()));
        }
        Ok(r.body)
    }

    /// Adds a user to the project of an address, as the system, with these roles and the
    /// address as login, and sends them an invitation (with the host's SignIn, if any). An
    /// existing user of that address is invited again, unchanged. `{id, login, created, invited}`.
    pub async fn add_user(&self, host: &str, email: &str, roles: Vec<String>) -> Result<J, (u16, String)> {
        let t = self.app.tenant(host).map_err(|_| (404, format!("no project is served at `{host}`")))?;
        let email = email.to_string();
        let mut r = t.ask(|reply| Job::AddUser { email, roles, reply }).await.ok_or((503, "the engine is not available".to_string()))?;
        if r.status != 200 {
            return Err((r.status, r.body["error"].as_str().unwrap_or("failed").to_string()));
        }
        let mut invited = false;
        if let (Some(send), Some(sign_in)) = (take_send(&mut r.body).filter(|s| !s.is_null()), self.app.sign_in.clone()) {
            let mail = SignInMail::Invite { to: send["to"].as_str().unwrap_or("").into(), login: send["login"].as_str().unwrap_or("").into() };
            sign_in.send(&t.id, host, mail).await.map_err(|e| (503, format!("the user is added, but the invitation could not be sent: {e}")))?;
            invited = true;
        }
        r.body["invited"] = json!(invited);
        Ok(r.body)
    }

    /// The tenants running now.
    pub fn running(&self) -> Vec<TenantStatus> {
        let now = Instant::now();
        let mut out: Vec<TenantStatus> = self
            .app
            .running
            .lock()
            .unwrap()
            .values()
            .map(|t| {
                let clock = t.reads_clock.load(std::sync::atomic::Ordering::Relaxed);
                let next_sweep = t.sweep_every.filter(|_| clock).map(|every| (*t.last_sweep.lock().unwrap() + every).saturating_duration_since(now));
                TenantStatus { id: t.id.clone(), idle: now.duration_since(*t.last_used.lock().unwrap()), next_sweep, engines: t.engines() }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Serves on `addr` until the process stops.
    pub async fn serve(&self, addr: std::net::SocketAddr) -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        axum::serve(listener, self.router()).await.map_err(|e| e.to_string())
    }
}

/// Start the engine and serve one project until the process stops (`quadra serve`).
pub fn serve(cfg: Config) -> Result<(), String> {
    serve_with(cfg, None)
}

/// `serve`, with every commit also kept in `log_dir` (one folder per tenant) before it is
/// confirmed.
pub fn serve_with(cfg: Config, log_dir: Option<std::path::PathBuf>) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let durability: Option<Arc<dyn Durability>> = log_dir.map(|d| Arc::new(LogFolder(d)) as Arc<dyn Durability>);
        let options = HostOptions { dev_login: cfg.dev_login, secure_cookies: cfg.secure_cookies, web: cfg.web.clone(), sign_in: None, requests_per_second: None, durability };
        let host = Host::new(options, Arc::new(Single(TenantSpec { id: "default".into(), config: cfg.clone() })));
        host.start("").await?;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.port));
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        eprintln!("quadra: serving on http://{addr}{}", if cfg.dev_login { " (development login enabled)" } else { "" });
        axum::serve(listener, host.router()).await.map_err(|e| e.to_string())
    })
}

/// The files of a project that matter, with their modification time and size.
fn signature(root: &std::path::Path) -> Vec<(std::path::PathBuf, std::time::SystemTime, u64)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "csv" | "toml" | "lock")) {
                if let Ok(m) = e.metadata() {
                    out.push((p, m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()));
                }
            }
        }
    }
    out.sort();
    out
}

mod webapp {
    // web/dist, embedded at build time (crates/quadra-server/build.rs)
    include!(concat!(env!("OUT_DIR"), "/webapp.rs"));
}

/// The embedded web client: its files, and `index.html` for every other path (routing in the page).
async fn embedded(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (name, bytes) = webapp::FILES.iter().find(|(p, _)| *p == path).or_else(|| webapp::FILES.iter().find(|(p, _)| *p == "index.html")).copied().unwrap_or(("", b"" as &[u8]));
    let kind = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    let cache = if name == "index.html" { "no-cache" } else { "public, max-age=31536000, immutable" };
    ([(axum::http::header::CONTENT_TYPE, kind), (axum::http::header::CACHE_CONTROL, cache)], bytes).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 28. `?since=` replays the backlog; the live copy of a replayed commit is skipped.
    #[test]
    fn a_commit_is_delivered_once() {
        let commit = |tx: u64| json!({ "tx": tx, "records": [] });
        let mut last = 3;
        // backlog 4, 5; then live 5 (already replayed), 6
        let delivered: Vec<u64> = [commit(4), commit(5), commit(5), commit(6), commit(2)].iter().filter(|e| fresh(e, &mut last)).map(|e| e["tx"].as_u64().unwrap()).collect();
        assert_eq!(delivered, vec![4, 5, 6]);
        assert!(fresh(&json!({ "type": "deploy", "sha": "x" }), &mut last));
        // without `since`, every live commit passes
        let mut last = 0;
        assert!(fresh(&commit(1), &mut last) && fresh(&commit(2), &mut last));
    }
}
