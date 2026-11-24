//! A host of several projects in one process (what Quadra Cloud builds on): each address
//! reaches its own engine and database, the limits of one tenant do not hold up another, a
//! stopped tenant leaves its SQLite file complete and starts again on the next request, and
//! starting and stopping a tenant many times leaks neither memory nor files.

use quadra_sema::Project;
use quadra_server::{Config, Host, HostOptions, Limits, TenantResolver, TenantSpec};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ITEMS: &str = "entity Item:\n    n: Int\n\nfn burn(n: Int) -> Int:\n    if n == 0:\n        return 1\n    return burn(n - 1) + burn(n - 1)\n";
const DUES: &str = "entity Due:\n    on: Date\n    late = on < today()\n";

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// A project in a folder of its own with its database initialized, and a token for user 1.
fn project(dir: &Path, code: &str) -> (Config, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"t\"\n").unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, now()).unwrap();
    let (token, _) = auth::create(&mut Conn::open(&url).unwrap(), "token", 1, "test", false, now(), None).unwrap();
    let cfg = Config { root: dir.to_path_buf(), url, port: 0, dev_login: false, secure_cookies: false, cache_mb: 16, prewarm_days: None, web: None, sweep_every: 3600, editors: vec![], watch: false, limits: Limits::default(), engines: Default::default() };
    (cfg, token)
}

struct Map(HashMap<String, TenantSpec>);

impl TenantResolver for Map {
    fn resolve(&self, host: &str) -> Option<TenantSpec> {
        self.0.get(host).cloned()
    }
}

static KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn http(port: u16, host: &str, token: &str, method: &str, path: &str, body: Option<J>) -> (u16, J) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: k{}-{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", std::process::id(), KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed), body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, serde_json::from_str(rest).unwrap_or(J::Null))
}

fn items(port: u16, host: &str, token: &str, columns: J) -> (u16, J) {
    http(port, host, token, "POST", "/v1/query", Some(json!({ "entity": "Item", "fields": ["n"], "columns": columns })))
}

/// A host on its own runtime, serving on a free port.
fn serve(specs: Vec<(&str, TenantSpec)>) -> (Host, tokio::runtime::Runtime, u16) {
    let host = Host::new(HostOptions::default(), Arc::new(Map(specs.into_iter().map(|(h, s)| (h.to_string(), s)).collect())));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let h = host.clone();
    rt.spawn(async move { h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await.unwrap() });
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (host, rt, port)
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-host-{name}-{}", std::process::id()))
}

#[test]
fn two_tenants_in_one_process_are_isolated() {
    let (a, ta) = project(&temp("a"), ITEMS);
    let (b, tb) = project(&temp("b"), ITEMS);
    let dues = project(&temp("dues"), DUES).0;
    let a = Config { limits: Limits { job: Some(Duration::from_millis(500)), ..Limits::default() }, ..a };
    let (host, rt, port) = serve(vec![
        ("a.test", TenantSpec { id: "a".into(), config: a.clone() }),
        ("b.test", TenantSpec { id: "b".into(), config: b }),
        ("dues.test", TenantSpec { id: "dues".into(), config: dues }),
    ]);

    // an unknown address
    assert_eq!(http(port, "nobody.test", &ta, "GET", "/v1/me", None).0, 404);
    // each tenant has its own database and its own tokens
    let (s, r) = http(port, "a.test", &ta, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": 7 } }] })));
    assert_eq!(s, 200, "{r}");
    assert_eq!(items(port, "a.test", &ta, json!({})).1["rows"].as_array().unwrap().len(), 1);
    assert_eq!(items(port, "b.test", &tb, json!({})).1["rows"].as_array().unwrap().len(), 0);
    assert_eq!(http(port, "b.test", &ta, "GET", "/v1/me", None).0, 401, "a token of a is not one of b");

    // a request of `a` past its limit does not hold up `b`
    let slow = std::thread::spawn(move || {
        let t = Instant::now();
        let r = items(port, "a.test", &ta, json!({ "x": "burn(60)" }));
        (r, t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    assert_eq!(items(port, "b.test", &tb, json!({ "x": "burn(3)" })).0, 200);
    let fast = t.elapsed();
    let ((s, r), took) = slow.join().unwrap();
    assert_eq!(s, 422, "{r}");
    assert!(r["error"].as_str().unwrap().starts_with("timeout"), "{r}");
    assert!(took < Duration::from_millis(550 + 150), "a answered after {took:?}");
    assert!(fast < Duration::from_millis(200), "b answered after {fast:?}, while a was busy");

    // what each tenant tells its host: nothing of `a` changes with time, `dues` does
    rt.block_on(host.start("dues.test")).unwrap();
    let running = host.running();
    let of = |id: &str| running.iter().find(|t| t.id == id).cloned().unwrap();
    assert_eq!(of("a").next_sweep, None);
    assert!(of("dues").next_sweep.is_some_and(|d| d <= Duration::from_secs(3600)));
    assert!(of("a").idle < Duration::from_secs(5));

    // stopped: the SQLite file holds every commit, and the next request starts it again
    assert!(rt.block_on(host.stop("a")));
    assert!(!rt.block_on(host.stop("a")), "already stopped");
    assert!(host.running().iter().all(|t| t.id != "a"));
    let wal = a.root.join("data.db-wal");
    assert!(std::fs::metadata(&wal).map_or(true, |m| m.len() == 0), "the log was written back into the database file");
    let (_, ta2) = (0, auth::create(&mut Conn::open(&a.url).unwrap(), "token", 1, "again", false, now(), None).unwrap().0);
    assert_eq!(items(port, "a.test", &ta2, json!({})).1["rows"][0]["cells"]["n"]["d"], json!("7"));
    assert!(host.running().iter().any(|t| t.id == "a"));
}

fn open_files() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

#[test]
fn starting_and_stopping_a_tenant_a_thousand_times_leaks_nothing() {
    let (cfg, _) = project(&temp("cycles"), ITEMS);
    let host = Host::new(HostOptions::default(), Arc::new(Map([("c.test".to_string(), TenantSpec { id: "c".into(), config: cfg })].into())));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let cycle = || {
        rt.block_on(host.start("c.test")).unwrap();
        assert!(rt.block_on(host.stop("c")));
    };
    for _ in 0..100 {
        cycle();
    }
    let (files, mem) = (open_files(), rss_mb());
    for _ in 0..900 {
        cycle();
    }
    // the background tasks of the stopped tenants notice within a second
    std::thread::sleep(Duration::from_millis(1200));
    let (files_after, mem_after) = (open_files(), rss_mb());
    eprintln!("after 100 cycles: {files} open files, {mem:.0} MB; after 1000: {files_after} open files, {mem_after:.0} MB");
    assert!(files_after <= files + 2, "open files: {files} after 100 cycles, {files_after} after 1000");
    assert!(mem_after < mem + 40.0, "memory: {mem:.0} MB after 100 cycles, {mem_after:.0} MB after 1000");
}

#[test]
fn a_full_queue_refuses_at_once_and_drains() {
    let (cfg, token) = project(&temp("queue"), ITEMS);
    let cfg = Config { limits: Limits { job: Some(Duration::from_millis(400)), max_queue: Some(3), ..Limits::default() }, ..cfg };
    let (_host, _rt, port) = serve(vec![("q.test", TenantSpec { id: "q".into(), config: cfg })]);
    let (s, r) = http(port, "q.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": 1 } }] })));
    assert_eq!(s, 200, "{r}");
    // more slow requests at once than may wait
    let slow: Vec<_> = (0..8)
        .map(|_| {
            let token = token.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                let (s, _) = items(port, "q.test", &token, json!({ "x": "burn(60)" }));
                (s, t.elapsed())
            })
        })
        .collect();
    let answers: Vec<(u16, Duration)> = slow.into_iter().map(|h| h.join().unwrap()).collect();
    let busy: Vec<_> = answers.iter().filter(|(s, _)| *s == 503).collect();
    assert!(!busy.is_empty(), "{answers:?}");
    assert!(busy.iter().all(|(_, d)| *d < Duration::from_millis(300)), "refused at once: {answers:?}");
    assert!(answers.iter().all(|(s, _)| *s == 503 || *s == 422), "{answers:?}");
    // the backlog drained: served again
    assert_eq!(items(port, "q.test", &token, json!({})).0, 200);
}

#[test]
fn readers_serve_at_once_and_see_every_commit() {
    use quadra_server::Engines;
    let (cfg, token) = project(&temp("readers"), ITEMS);
    let cfg = Config { limits: Limits { job: Some(Duration::from_millis(1500)), ..Limits::default() }, engines: Engines { readers: 3, writers: 1 }, ..cfg };
    let (host, _rt, port) = serve(vec![("r.test", TenantSpec { id: "r".into(), config: cfg })]);
    let (s, r) = http(port, "r.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": 0 } }] })));
    assert_eq!(s, 200, "{r}");
    // the readers start after the first engine
    for _ in 0..100 {
        if host.running().first().is_some_and(|t| t.engines == 4) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(host.running()[0].engines, 4, "a writer and three readers");

    // two heavy reads at once: they take two readers; a third reader and the writer stay free
    let heavy: Vec<_> = (0..2)
        .map(|_| {
            let token = token.clone();
            std::thread::spawn(move || items(port, "r.test", &token, json!({ "x": "burn(60)" })).0)
        })
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    assert_eq!(items(port, "r.test", &token, json!({})).0, 200);
    let read = t.elapsed();
    let t = Instant::now();
    let (s, r) = http(port, "r.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": 1 } }] })));
    let write = t.elapsed();
    assert_eq!(s, 200, "{r}");
    assert!(read < Duration::from_millis(200), "a read waited {read:?} behind the heavy ones");
    assert!(write < Duration::from_millis(200), "a write waited {write:?} behind the heavy reads");
    for h in heavy {
        assert_eq!(h.join().unwrap(), 422, "the heavy reads stop at their limit");
    }

    // every write is seen by the read that follows it, whichever reader serves it
    for n in 2..42 {
        let (s, r) = http(port, "r.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": n } }] })));
        assert_eq!(s, 200, "{r}");
        let rows = items(port, "r.test", &token, json!({})).1["rows"].as_array().unwrap().len();
        assert_eq!(rows, n as usize + 1, "after the commit of item {n}");
    }
}

#[test]
fn a_base_guards_the_records_a_request_changes_and_nothing_else() {
    let (cfg, token) = project(&temp("base"), ITEMS);
    let (_host, _rt, port) = serve(vec![("b.test", TenantSpec { id: "b".into(), config: cfg })]);
    let create = |n: i64| http(port, "b.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Item", "values": { "n": n } }] })));
    let (_, a) = create(1);
    let (_, b) = create(2);
    let id = |r: &J| r["created"].as_object().and_then(|c| c.values().next()).and_then(|v| v.as_u64()).or_else(|| r["created"]["0"].as_u64());
    let (a, b) = (id(&a).unwrap_or(0), id(&b).unwrap_or(0));
    let rows = items(port, "b.test", &token, json!({})).1;
    let ids: Vec<u64> = rows["rows"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()).collect();
    let (a, b) = if a > 0 && b > 0 { (a, b) } else { (ids[0], ids[1]) };
    let base = rows["base"].as_u64().unwrap();
    let edit = |id: u64, n: i64| http(port, "b.test", &token, "POST", "/v1/tx", Some(json!({ "base": base, "ops": [{ "op": "update", "entity": "Item", "id": id, "values": { "n": n } }] })));
    // two records changed from the same base: both written
    assert_eq!(edit(a, 10).0, 200);
    let (s, r) = edit(b, 20);
    assert_eq!(s, 200, "{r}");
    // the same record again from that base: the change since would be overwritten unseen
    let (s, r) = edit(a, 11);
    assert_eq!(s, 409, "{r}");
    assert!(r["conflicts"][0]["what"][0].as_str().unwrap().ends_with(".n"), "{r}");
}

/// A host that woke a sleeping tenant because a sweep was due runs it at once and waits.
#[test]
fn a_sweep_on_demand_runs_at_once() {
    const NOTES: &str = "entity Note:\n    text: Text\n";
    let dues = project(&temp("sweep-dues"), DUES).0;
    let notes = project(&temp("sweep-notes"), NOTES).0;
    let (host, rt, _) = serve(vec![("dues.test", TenantSpec { id: "dues".into(), config: dues }), ("notes.test", TenantSpec { id: "notes".into(), config: notes })]);
    // it starts the tenant if needed
    assert!(host.running().is_empty());
    assert_eq!(rt.block_on(host.sweep("dues.test")), Ok(None), "nothing to change in an empty project");
    let of = |id: &str| host.running().into_iter().find(|t| t.id == id).unwrap();
    assert!(of("dues").next_sweep.is_some_and(|d| d > Duration::from_secs(3590)), "the next sweep counts from this one");
    // a project without time-driven rules sweeps too (nothing to do)
    assert_eq!(rt.block_on(host.sweep("notes.test")), Ok(None));
    assert_eq!(of("notes").next_sweep, None);
    assert!(rt.block_on(host.sweep("nobody.test")).unwrap_err().contains("no project"));
}

/// Past the size limit a write fails with the limit as its error, and nothing of it is
/// written; the tenant goes on serving reads.
#[test]
fn a_database_stops_growing_at_its_size_limit() {
    const NOTES: &str = "entity Note:\n    text: Text\n";
    let (cfg, token) = project(&temp("size"), NOTES);
    let start = std::fs::metadata(cfg.root.join("data.db")).unwrap().len();
    let max = 1 << 20;
    assert!(start < max / 2, "an empty project takes {start} bytes");
    let cfg = Config { limits: Limits { max_bytes: Some(max), ..Limits::default() }, ..cfg };
    let (_host, _rt, port) = serve(vec![("size.test", TenantSpec { id: "size".into(), config: cfg })]);
    let text = "x".repeat(8 << 10);
    let mut written = 0;
    let err = loop {
        let (s, r) = http(port, "size.test", &token, "POST", "/v1/tx", Some(json!({ "ops": [{ "op": "create", "entity": "Note", "values": { "text": text } }] })));
        if s != 200 {
            break r;
        }
        written += 1;
        assert!(written < 400, "the limit was never reached");
    };
    assert_eq!(err["error"], json!("too large: the database reached its size limit of 1 MB (nothing of this request was written)"));
    assert!(written >= 8, "only {written} notes fit");
    let (s, r) = http(port, "size.test", &token, "POST", "/v1/query", Some(json!({ "entity": "Note", "fields": ["text"] })));
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["rows"].as_array().unwrap().len(), written, "the refused write left nothing");
}
