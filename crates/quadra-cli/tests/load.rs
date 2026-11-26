//! Many people at once on one project: a host serves the `stress` world (`quadra bench gen`) to
//! hundreds of simulated users over HTTP, each with its own token, pausing between requests
//! as a person does. The same load runs with different numbers of readers; the table says how
//! long each kind of request took and how much memory the engines used.
//!
//! Not run by `cargo test` (it needs a loaded world and takes minutes):
//!
//!   quadra bench run examples/stress --url sqlite:/tmp/medium.db --scale medium --only load
//!   QUADRA_LOAD_URL=sqlite:/tmp/medium.db cargo test --release -p quadra-cli --test load -- --ignored --nocapture
//!
//! QUADRA_LOAD_USERS (300), QUADRA_LOAD_SECONDS (30), QUADRA_LOAD_THINK_MS (2000, the mean
//! pause), QUADRA_LOAD_READERS (`0,2,4`), QUADRA_LOAD_WRITERS (1; more only on PostgreSQL).

use quadra_server::{Config, Engines, Host, HostOptions, Limits, TenantResolver, TenantSpec};
use quadra_store::conn::{Conn, Sql};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

struct One(TenantSpec);

impl TenantResolver for One {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

static KEYS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// the error of the last request of this thread
    static LAST_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// One request; its status and body.
fn http(port: u16, token: &str, method: &str, path: &str, body: Option<&J>) -> (u16, J) {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return (0, J::Null);
    };
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let key = KEYS.fetch_add(1, Ordering::Relaxed);
    let req = format!("{method} {path} HTTP/1.1\r\nHost: load.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: load-{}-{key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", std::process::id(), body.len());
    if s.write_all(req.as_bytes()).is_err() {
        return (0, J::Null);
    }
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    // what the users need of an answer: its base, and the message of an error (a person's
    // browser parses the rest; here it would only take the CPU from the server)
    let base = rest.find("\"base\":").and_then(|i| rest[i + 7..].trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok());
    let error = if status >= 400 { serde_json::from_str::<J>(rest).ok().map(|b| b["error"].clone()) } else { None };
    LAST_ERROR.with(|e| *e.borrow_mut() = error.as_ref().map(|x| x.to_string()));
    (status, json!({ "base": base, "error": error }))
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() % 1_000_000) as f64 / 1_000_000.0
    }

    /// A pause with this mean, as people's pauses go (exponential).
    fn pause(&mut self, mean_ms: f64) -> Duration {
        Duration::from_millis((-(1.0 - self.unit()).ln() * mean_ms).min(mean_ms * 6.0) as u64)
    }
}

/// What the world offers the users: whom to log in as, what to open, what to change.
struct World {
    tokens: Vec<String>,
    orders: Vec<u64>,
    /// one order line per user (no two users change the same line)
    lines: Vec<(u64, f64)>,
}

fn world(url: &str, users: usize) -> World {
    let mut conn = Conn::open(url).unwrap();
    let one = |conn: &mut Conn, sql: &str| -> Vec<Vec<Option<String>>> { conn.query(sql, &[]).unwrap() };
    let admin: u64 = one(&mut conn, "SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = 'admin'")[0][0].as_deref().unwrap().parse().unwrap();
    let now = chrono::Local::now().naive_local();
    let tokens = (0..users).map(|i| quadra_store::auth::create(&mut conn, "token", admin, &format!("load-{}-{i}", std::process::id()), false, now, None).unwrap().0).collect();
    let orders = one(&mut conn, "SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" = 'Confirmed' ORDER BY \"id\" LIMIT 2000").iter().map(|r| r[0].as_deref().unwrap().parse().unwrap()).collect();
    let lines = conn
        .query(
            "SELECT CAST(l.\"id\" AS TEXT), CAST(l.\"qty\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' ORDER BY l.\"id\" LIMIT ?bigint",
            &[Sql::Int(users as i64)],
        )
        .unwrap()
        .iter()
        .map(|r| (r[0].as_deref().unwrap().parse().unwrap(), r[1].as_deref().unwrap().parse().unwrap()))
        .collect();
    World { tokens, orders, lines }
}

#[derive(Default)]
struct Kind {
    ms: Vec<f64>,
    errors: usize,
    conflicts: usize,
    statuses: BTreeMap<u16, usize>,
    first_error: Option<String>,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// The load with `readers` readers: per kind of request, its latencies.
fn run(world: &Arc<World>, url: &str, readers: usize, writers: usize) {
    let (users, seconds, think) = (world.tokens.len(), env("QUADRA_LOAD_SECONDS", 30u64), env("QUADRA_LOAD_THINK_MS", 2000.0f64));
    // the share of requests that commit (the rest of the 10% becomes searches)
    let writes = env("QUADRA_LOAD_WRITES", 0.10f64).clamp(0.0, 0.10);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let config = Config {
        root,
        url: url.to_string(),
        port: 0,
        dev_login: false,
        secure_cookies: false,
        cache_mb: 256,
        prewarm_days: None,
        web: None,
        sweep_every: 0,
        editors: vec![],
        watch: false,
        limits: Limits { job: Some(Duration::from_secs(30)), max_queue: None, ..Limits::default() },
        engines: Engines { readers, writers },
    };
    let host = Host::new(HostOptions::default(), Arc::new(One(TenantSpec { id: "load".into(), config })));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let h = host.clone();
    rt.spawn(async move { h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await.unwrap() });
    rt.block_on(host.start("load.test")).unwrap();
    let before = rss_mb();
    let wanted = readers + if url.starts_with("sqlite:") { 1 } else { writers.max(1) };
    for _ in 0..600 {
        if host.running().first().is_some_and(|t| t.engines >= wanted) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // every engine warmed up a little: a read for each, one write
    for t in world.tokens.iter().take(readers + 2) {
        let _ = http(port, t, "POST", "/v1/query", Some(&json!({ "entity": "Shipment", "limit": 500, "offset": 0 })));
    }

    let stats: Arc<Mutex<BTreeMap<&'static str, Kind>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let end = Instant::now() + Duration::from_secs(seconds);
    let threads: Vec<_> = (0..users)
        .map(|u| {
            let (world, stats) = (world.clone(), stats.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (u as u64 + 1) * 0x1000_0001);
                let token = &world.tokens[u];
                let (line, qty) = world.lines[u % world.lines.len()];
                let mut n = 0u64;
                // start at different moments, having read the line one will change
                std::thread::sleep(rng.pause(think));
                let mut base: Option<u64> = http(port, token, "GET", &format!("/v1/records/OrderLine/{line}"), None).1["base"].as_u64();
                while Instant::now() < end {
                    let roll = rng.unit();
                    let t = Instant::now();
                    let (kind, status): (&'static str, u16) = if roll < 0.45 {
                        let o = world.orders[(rng.next() as usize) % world.orders.len()];
                        let (s, b) = http(port, token, "GET", &format!("/v1/records/Order/{o}"), None);
                        base = b["base"].as_u64().or(base);
                        ("record", s)
                    } else if roll < 0.70 {
                        let (s, b) = http(port, token, "POST", "/v1/query", Some(&json!({ "entity": "Shipment", "limit": 500, "offset": 0 })));
                        base = b["base"].as_u64().or(base);
                        ("grid", s)
                    } else if roll < 0.90 - writes {
                        ("search", http(port, token, "POST", "/v1/query", Some(&json!({ "entity": "Customer", "fields": ["code", "name"], "where": "name.starts_with(\"Rossi\")", "limit": 50 }))).0)
                    } else if roll < 0.90 {
                        n += 1;
                        let q = if n % 2 == 1 { qty + 1.0 } else { qty };
                        let body = json!({ "base": base.unwrap_or(0), "ops": [{ "op": "update", "entity": "OrderLine", "id": line, "values": { "qty": format!("{q} pcs") } }] });
                        let (s, b) = http(port, token, "POST", "/v1/tx", Some(&body));
                        base = b["base"].as_u64().or(base);
                        if s == 409 {
                            // read again, then the change once more
                            let (_, r) = http(port, token, "GET", &format!("/v1/records/OrderLine/{line}"), None);
                            base = r["base"].as_u64().or(base);
                        }
                        ("commit", s)
                    } else {
                        let q = if rng.unit() < 0.5 {
                            json!({ "entity": "Order", "where": "state in {Confirmed, Shipped}", "group_by": ["customer.region", { "field": "date", "bucket": "month" }], "aggregates": { "total": "sum" }, "limit": 100 })
                        } else {
                            json!({ "entity": "Product", "group_by": ["category", "active"], "pivot": true, "aggregates": { "units_sold": "sum", "margin": "sum" }, "limit": 100 })
                        };
                        ("report", http(port, token, "POST", "/v1/query", Some(&q)).0)
                    };
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    {
                        let mut s = stats.lock().unwrap();
                        let k = s.entry(kind).or_default();
                        k.ms.push(ms);
                        if status == 409 {
                            k.conflicts += 1;
                        } else if !(200..300).contains(&status) {
                            k.errors += 1;
                            *k.statuses.entry(status).or_default() += 1;
                            if k.first_error.is_none() {
                                k.first_error = LAST_ERROR.with(|e| e.borrow().clone());
                            }
                        }
                    }
                    std::thread::sleep(rng.pause(think));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let after = rss_mb();
    let engines = host.running().first().map_or(0, |t| t.engines);
    rt.block_on(host.stop("load"));

    let mut stats = stats.lock().unwrap();
    let total: usize = stats.values().map(|k| k.ms.len()).sum();
    println!("\n{users} users, {readers} readers, {writers} writer(s) ({engines} engines), {seconds} s: {:.0} requests/s, memory {before:.0} → {after:.0} MB", total as f64 / seconds as f64);
    println!("{:<8} {:>7} {:>9} {:>9} {:>9} {:>9} {:>7} {:>9}", "kind", "count", "p50 ms", "p95 ms", "p99 ms", "max ms", "errors", "conflicts");
    let mut reads = Vec::new();
    for (name, k) in stats.iter_mut() {
        if *name != "commit" {
            reads.extend(k.ms.iter().copied());
        }
        let (p50, p95, p99) = (pct(&mut k.ms, 0.5), pct(&mut k.ms, 0.95), pct(&mut k.ms, 0.99));
        let max = k.ms.iter().cloned().fold(0.0, f64::max);
        println!("{name:<8} {:>7} {p50:>9.1} {p95:>9.1} {p99:>9.1} {max:>9.1} {:>7} {:>9} {}", k.ms.len(), k.errors, k.conflicts, if k.statuses.is_empty() { String::new() } else { format!("{:?}", k.statuses) });
    }
    println!("{:<8} {:>7} {:>9.1} {:>9.1} {:>9.1}", "reads", reads.len(), pct(&mut reads, 0.5), pct(&mut reads, 0.95), pct(&mut reads, 0.99));
    for (name, k) in stats.iter() {
        if let Some(e) = &k.first_error {
            println!("first error of {name}: {}", e.chars().take(300).collect::<String>());
        }
    }
}

#[test]
#[ignore]
fn hundreds_of_people_at_once() {
    let Ok(url) = std::env::var("QUADRA_LOAD_URL") else {
        eprintln!("QUADRA_LOAD_URL not set: see the top of tests/load.rs");
        return;
    };
    let users = env("QUADRA_LOAD_USERS", 300usize);
    let writers = env("QUADRA_LOAD_WRITERS", 1usize);
    let world = Arc::new(world(&url, users));
    for readers in env("QUADRA_LOAD_READERS", "0,2,4".to_string()).split(',').filter_map(|r| r.trim().parse().ok()) {
        run(&world, &url, readers, writers);
    }
}

/// One commit alone, as its base gets older: what a commit costs when others committed since
/// the user read (`QUADRA_LOAD_URL` as above).
#[test]
#[ignore]
fn a_commit_on_an_older_base() {
    let Ok(url) = std::env::var("QUADRA_LOAD_URL") else {
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let mut conn = Conn::open(&url).unwrap();
    let admin: u64 = conn.query("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = 'admin'", &[]).unwrap()[0][0].as_deref().unwrap().parse().unwrap();
    let lines: Vec<(u64, f64)> = conn
        .query("SELECT CAST(l.\"id\" AS TEXT), CAST(l.\"qty\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' ORDER BY l.\"id\" LIMIT 2000 OFFSET 5000", &[])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_deref().unwrap().parse().unwrap(), r[1].as_deref().unwrap().parse().unwrap()))
        .collect();
    let mut local = quadra_server::Local::open(&root, &url, 256).unwrap();
    let mut next = 0usize;
    let mut commit = |local: &mut quadra_server::Local, base: u64| -> (u16, f64, u64) {
        let (line, qty) = lines[next % lines.len()];
        next += 1;
        let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "OrderLine", "id": line, "values": { "qty": format!("{} pcs", qty + 1.0) } }] });
        let t = Instant::now();
        let (s, r) = local.tx(admin, &body, &format!("age-{}-{next}", std::process::id()));
        if s == 409 && std::env::var("QUADRA_LOAD_SHOW").is_ok() {
            println!("409 on OrderLine#{line}: {}", r["conflicts"].to_string().chars().take(700).collect::<String>());
        }
        (s, t.elapsed().as_secs_f64() * 1000.0, r["base"].as_u64().unwrap_or(base))
    };
    let mut head = quadra_store::source::head(&mut conn).unwrap();
    println!("\n{:>6} {:>9} {:>7}", "behind", "ms", "status");
    for behind in [0u64, 1, 10, 50, 200] {
        // `behind` commits by others, then one on the base read before them
        let base = head;
        for _ in 0..behind {
            head = commit(&mut local, head).2;
        }
        let (s, ms, h) = commit(&mut local, base);
        head = h.max(head);
        println!("{behind:>6} {ms:>9.1} {s:>7}");
    }
}

/// A report before and after a commit (`QUADRA_LOAD_URL` as above): computed in SQL both
/// times, and how long it takes.
#[test]
#[ignore]
fn a_report_after_a_commit() {
    let Ok(url) = std::env::var("QUADRA_LOAD_URL") else {
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let mut conn = Conn::open(&url).unwrap();
    let admin: u64 = conn.query("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = 'admin'", &[]).unwrap()[0][0].as_deref().unwrap().parse().unwrap();
    let line: (u64, f64) = conn
        .query("SELECT CAST(l.\"id\" AS TEXT), CAST(l.\"qty\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' ORDER BY l.\"id\" LIMIT 1 OFFSET 9000", &[])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_deref().unwrap().parse().unwrap(), r[1].as_deref().unwrap().parse().unwrap()))
        .next()
        .unwrap();
    let mut local = quadra_server::Local::open(&root, &url, 256).unwrap();
    let reports = [
        ("group", json!({ "entity": "Order", "where": "state in {Confirmed, Shipped}", "group_by": ["customer.region", { "field": "date", "bucket": "month" }], "aggregates": { "total": "sum" }, "limit": 100 })),
        ("pivot", json!({ "entity": "Product", "group_by": ["category", "active"], "pivot": true, "aggregates": { "units_sold": "sum", "margin": "sum" }, "limit": 100 })),
        ("grid", json!({ "entity": "Shipment", "limit": 500, "offset": 0 })),
    ];
    let show = |local: &mut quadra_server::Local, when: &str| {
        for (name, q) in &reports {
            let t = Instant::now();
            let (s, b) = local.query(admin, q);
            let (_, misses, len, bytes) = local.cache();
            println!("{when:<14} {name:<6} {s} sql={} {:>8.1} ms  cache: {len} records, {} MB, {misses} loaded{}", b["sql"], t.elapsed().as_secs_f64() * 1000.0, bytes >> 20, if s != 200 { format!(" {}", b.to_string().chars().take(200).collect::<String>()) } else { String::new() });
        }
    };
    show(&mut local, "before");
    show(&mut local, "again");
    let base = quadra_store::source::head(&mut conn).unwrap();
    for n in 0..3 {
        let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "OrderLine", "id": line.0, "values": { "qty": format!("{} pcs", line.1 + 1.0 + n as f64) } }] });
        let (s, r) = local.tx(admin, &body, &format!("report-{}-{n}", std::process::id()));
        println!("commit {n}: {s} {}", if s != 200 { r.to_string() } else { String::new() });
        show(&mut local, &format!("after commit {n}"));
    }
}

/// Writers at full speed on PostgreSQL (`QUADRA_LOAD_URL` a postgres:// url of the loaded
/// world): 32 clients commit without pausing, on lines of different customers ("spread") or on
/// lines of one customer's orders ("hot": the same customer's and region's totals change in
/// every commit), with 1, 4 and 16 writing engines. How many commits per second, how long
/// they take, how many come back refused.
#[test]
#[ignore]
fn writers_at_full_speed() {
    let Ok(url) = std::env::var("QUADRA_LOAD_URL") else {
        return;
    };
    let clients = env("QUADRA_LOAD_CLIENTS", 32usize);
    let seconds = env("QUADRA_LOAD_SECONDS", 20u64);
    let mut conn = Conn::open(&url).unwrap();
    let admin: u64 = conn.query("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = 'admin'", &[]).unwrap()[0][0].as_deref().unwrap().parse().unwrap();
    let now = chrono::Local::now().naive_local();
    let tokens: Vec<String> = (0..clients).map(|i| quadra_store::auth::create(&mut conn, "token", admin, &format!("writers-{}-{i}", std::process::id()), false, now, None).unwrap().0).collect();
    let lines = |conn: &mut Conn, sql: &str| -> Vec<(u64, f64)> { conn.query(sql, &[]).unwrap().iter().map(|r| (r[0].as_deref().unwrap().parse().unwrap(), r[1].as_deref().unwrap().parse().unwrap())).collect() };
    // one line per customer, and the lines of the customer with the most
    let spread = lines(&mut conn, &format!("SELECT CAST(MIN(l.\"id\") AS TEXT), CAST(MIN(l.\"qty\") AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' GROUP BY o.\"customer_id\" LIMIT {clients}"));
    let hot = lines(&mut conn, &format!("SELECT CAST(l.\"id\" AS TEXT), CAST(l.\"qty\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE o.\"state\" = 'Confirmed' AND o.\"customer_id\" = (SELECT o2.\"customer_id\" FROM \"order\" o2 JOIN \"order_line\" l2 ON l2.\"order_id\" = o2.\"id\" WHERE o2.\"state\" = 'Confirmed' GROUP BY o2.\"customer_id\" ORDER BY COUNT(*) DESC LIMIT 1) LIMIT {clients}"));
    let tokens = Arc::new(tokens);
    println!("\n{clients} clients without pauses, {seconds} s each, {} spread lines, {} hot lines", spread.len(), hot.len());
    println!("{:<7} {:>8} {:>10} {:>9} {:>9} {:>9} {:>7} {:>7}", "lines", "writers", "commits/s", "p50 ms", "p95 ms", "p99 ms", "409", "other");
    for (mode, set) in [("spread", spread), ("hot", hot)] {
        let set = Arc::new(set);
        for writers in env("QUADRA_LOAD_WRITERS_LIST", "1,4,16".to_string()).split(',').filter_map(|w| w.trim().parse::<usize>().ok()) {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
            let config = Config {
                root,
                url: url.clone(),
                port: 0,
                dev_login: false,
                secure_cookies: false,
                cache_mb: 256,
                prewarm_days: None,
                web: None,
                sweep_every: 0,
                editors: vec![],
                watch: false,
                limits: Limits { job: Some(Duration::from_secs(30)), max_queue: None, ..Limits::default() },
                engines: Engines { readers: 0, writers },
            };
            // the engine at full speed: no limit per credential; QUADRA_LOAD_SINK_MS: every batch
            // also kept somewhere that takes that long (object storage), before it is confirmed
            let durability: Option<Arc<dyn quadra_server::Durability>> = std::env::var("QUADRA_LOAD_SINK_MS").ok().and_then(|v| v.parse().ok()).map(|ms| Arc::new(Slow(ms)) as Arc<dyn quadra_server::Durability>);
            let host = Host::new(HostOptions { requests_per_second: Some(1e9), durability, ..Default::default() }, Arc::new(One(TenantSpec { id: "writers".into(), config })));
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let h = host.clone();
            rt.spawn(async move { h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await.unwrap() });
            rt.block_on(host.start("load.test")).unwrap();
            for _ in 0..600 {
                if host.running().first().is_some_and(|t| t.engines >= writers) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let end = Instant::now() + Duration::from_secs(seconds);
            let results: Vec<(Vec<f64>, usize, usize, BTreeMap<String, usize>)> = (0..clients)
                .map(|c| {
                    let (tokens, set) = (tokens.clone(), set.clone());
                    std::thread::spawn(move || {
                        let (token, (line, qty)) = (&tokens[c], set[c % set.len()]);
                        let mut base = http(port, token, "GET", &format!("/v1/records/OrderLine/{line}"), None).1["base"].as_u64().unwrap_or(0);
                        let (mut ms, mut refused, mut other, mut n) = (Vec::new(), 0, 0, 0u64);
                        let mut why: BTreeMap<String, usize> = BTreeMap::new();
                        while Instant::now() < end {
                            n += 1;
                            let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "OrderLine", "id": line, "values": { "qty": format!("{} pcs", qty + (n % 2) as f64) } }] });
                            let t = Instant::now();
                            let (s, b) = http(port, token, "POST", "/v1/tx", Some(&body));
                            ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            match s {
                                200 => base = b["base"].as_u64().unwrap_or(base),
                                409 => {
                                    refused += 1;
                                    base = http(port, token, "GET", &format!("/v1/records/OrderLine/{line}"), None).1["base"].as_u64().unwrap_or(base);
                                }
                                _ => {
                                    other += 1;
                                    let m = format!("{s}: {}", b["error"].as_str().unwrap_or("").chars().take(90).collect::<String>());
                                    *why.entry(m).or_default() += 1;
                                }
                            }
                        }
                        (ms, refused, other, why)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect();
            rt.block_on(host.stop("writers"));
            let mut all: Vec<f64> = results.iter().flat_map(|r| r.0.iter().copied()).collect();
            let (refused, other): (usize, usize) = (results.iter().map(|r| r.1).sum(), results.iter().map(|r| r.2).sum());
            let ok = all.len() - refused - other;
            println!("{mode:<7} {writers:>8} {:>10.0} {:>9.1} {:>9.1} {:>9.1} {refused:>7} {other:>7}", ok as f64 / seconds as f64, pct(&mut all, 0.5), pct(&mut all, 0.95), pct(&mut all, 0.99));
            let mut why: BTreeMap<String, usize> = BTreeMap::new();
            for r in &results {
                for (k, v) in &r.3 {
                    *why.entry(k.clone()).or_default() += v;
                }
            }
            for (k, v) in why.iter().take(4) {
                println!("        {v:>7} × {k}");
            }
        }
    }
}

/// A destination that takes `ms` milliseconds per batch, as object storage does.
struct Slow(u64);

struct SlowSink(u64);

impl quadra_store::sequencer::DurabilitySink for SlowSink {
    fn persist(&mut self, _: &quadra_store::sequencer::Batch) -> Result<(), String> {
        std::thread::sleep(Duration::from_millis(self.0));
        Ok(())
    }
}

impl quadra_server::Durability for Slow {
    fn destinations(&self, _: &str) -> (Vec<Box<dyn quadra_store::sequencer::DurabilitySink>>, quadra_store::sequencer::Rule) {
        (vec![Box::new(SlowSink(self.0))], quadra_store::sequencer::Rule::All)
    }
}
