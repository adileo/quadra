//! The delivery of events with an `outbound`: a webhook arrives signed, once, with its
//! idempotency key; an answer that may change is tried again later, one that will not fails
//! for good; a host that only reaches public addresses does not reach this machine.

use quadra_sema::Project;
use quadra_server::{Config, Host, HostOptions, Limits, Outside, TenantResolver, TenantSpec};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CODE: &str = "entity Order:\n    number: Text\n\nevent OrderPlaced(number: Text)\n\non Order created as o:\n    emit OrderPlaced(number=o.number)\n\noutbound OrderPlaced via hooks path \"/orders\"\n";

/// A receiver of webhooks on a free port: each request kept, answered with the next status of
/// `answers` (the last one once they run out).
/// The requests a receiver got: their headers (lower case) and bodies.
type Got = Arc<Mutex<Vec<(HashMap<String, String>, String)>>>;

struct Receiver {
    port: u16,
    got: Got,
}

fn receiver(answers: Vec<u16>) -> Receiver {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        let mut answers = answers.into_iter().peekable();
        let mut last = 200;
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body) = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (String::new(), String::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((h, b)) = text.split_once("\r\n\r\n") {
                    let len: usize = h
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if b.len() >= len {
                        break (h.to_string(), b[..len].to_string());
                    }
                }
            };
            let mut headers: HashMap<String, String> = head
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            // the request line: `:method` and `:path`
            let mut first = head.lines().next().unwrap_or("").split_whitespace();
            headers.insert(":method".into(), first.next().unwrap_or("").to_string());
            headers.insert(":path".into(), first.next().unwrap_or("").to_string());
            g.lock().unwrap().push((headers, body));
            let status = match answers.next() {
                Some(s) => {
                    last = s;
                    s
                }
                None => last,
            };
            let extra = if status == 429 {
                "Retry-After: 120\r\n"
            } else {
                ""
            };
            let answer = if status == 200 {
                "{\"tracking\":\"TRK-42\",\"x\":1}"
            } else {
                "no"
            };
            let _ = s.write_all(
                format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len())
                    .as_bytes(),
            );
        }
    });
    Receiver { port, got }
}

struct One(TenantSpec);

impl TenantResolver for One {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-deliver-{name}-{}", std::process::id()))
}

/// The project in `dir`, its webhooks going to `port`, its database initialized; a token.
fn project(dir: &Path, port: u16) -> (Config, String) {
    project_with(dir, port, CODE)
}

fn project_with(dir: &Path, port: u16, code: &str) -> (Config, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"d\"\n\n[vars]\nHOOK_SECRET = {{ secret = true }}\n\n[connections.hooks]\nkind = \"http\"\nurl = \"http://127.0.0.1:{port}\"\ninsecure = true\nsigning_secret = {{ var = \"HOOK_SECRET\" }}\ntimeout = \"3s\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    ops::init(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        now(),
    )
    .unwrap();
    let (token, _) = auth::create(
        &mut Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now(),
        None,
    )
    .unwrap();
    let mut env = BTreeMap::new();
    env.insert("HOOK_SECRET".to_string(), "s3cret".to_string());
    let cfg = Config {
        outside: Outside { env, deliver: true },
        root: dir.to_path_buf(),
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
        limits: Limits::default(),
        engines: Default::default(),
    };
    (cfg, token)
}

fn serve(options: HostOptions, cfg: Config) -> (Host, tokio::runtime::Runtime, u16) {
    let host = Host::new(
        options,
        Arc::new(One(TenantSpec {
            id: "d".into(),
            config: cfg,
        })),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let h = host.clone();
    rt.spawn(async move {
        h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .unwrap()
    });
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (host, rt, port)
}

fn place_order(port: u16, token: &str, number: &str) {
    let body =
        json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": number } }] })
            .to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!("POST /v1/tx HTTP/1.1\r\nHost: d.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {number}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
}

/// The outbox row of the event, once `ready` says it is as expected (within 15 s).
fn outbox_row(cfg: &Config, ready: impl Fn(&[Option<String>]) -> bool) -> Vec<Option<String>> {
    let start = Instant::now();
    loop {
        let rows = Conn::open(&cfg.url)
            .unwrap()
            .query("SELECT CAST(\"attempts\" AS TEXT), \"delivered_at\", \"next_at\", \"error\", \"failed_at\" FROM \"_quadra_outbox\" ORDER BY \"id\"", &[])
            .unwrap();
        if let Some(r) = rows.into_iter().next() {
            if ready(&r) {
                return r;
            }
            if start.elapsed() > Duration::from_secs(15) {
                panic!("the outbox row is {r:?}");
            }
        } else if start.elapsed() > Duration::from_secs(15) {
            panic!("no outbox row");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_webhook_arrives_signed_once_and_is_marked_delivered() {
    let r = receiver(vec![200]);
    let dir = temp("ok");
    let (cfg, token) = project(&dir, r.port);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "A1");
    let row = outbox_row(&cfg, |r| r[1].is_some());
    assert_eq!(row[0].as_deref(), Some("1"));
    let log = quadra_store::deliveries::log(&mut Conn::open(&cfg.url).unwrap(), 1).unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(
        (log[0].outcome.as_str(), log[0].status),
        ("delivered", Some(200))
    );
    let got = r.got.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    let (h, body) = &got[0];
    let b: J = serde_json::from_str(body).unwrap();
    assert_eq!(b["event"], "OrderPlaced");
    assert_eq!(b["data"]["number"], "A1");
    assert_eq!(h["quadra-event"], "OrderPlaced");
    // the commit's hash and the event's number: the same on every attempt, another after a restore
    let key = &h["idempotency-key"];
    assert!(
        key.starts_with("quadra-") && key.ends_with(&format!("-{}", b["id"])),
        "{key}"
    );
    assert_eq!(
        key.len(),
        "quadra-".len() + 16 + 1 + b["id"].to_string().len(),
        "{key}"
    );
    // the signature: HMAC-SHA256 of "<t>.<body>" with the secret of the environment
    let sig = &h["quadra-signature"];
    let (t, v1) = sig.split_once(",v1=").unwrap();
    let t = t.strip_prefix("t=").unwrap();
    assert_eq!(
        v1,
        quadra_server::deliver::sign("s3cret", &format!("{t}.{body}"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unavailable_receiver_is_tried_again_and_a_refusal_fails_for_good() {
    // 503: tried again later, the error kept
    let r = receiver(vec![503]);
    let dir = temp("retry");
    let (cfg, token) = project(&dir, r.port);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "B1");
    let row = outbox_row(&cfg, |r| r[0].as_deref() == Some("1"));
    assert!(row[1].is_none() && row[4].is_none(), "{row:?}");
    assert!(row[2].is_some(), "a time to try again: {row:?}");
    assert!(row[3].as_deref().unwrap_or("").contains("503"), "{row:?}");
    drop(_host);
    let _ = std::fs::remove_dir_all(&dir);

    // 400: a refusal that will not change
    let r = receiver(vec![400]);
    let dir = temp("refused");
    let (cfg, token) = project(&dir, r.port);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "C1");
    let row = outbox_row(&cfg, |r| r[4].is_some());
    assert!(row[3].as_deref().unwrap_or("").contains("400"), "{row:?}");
    assert_eq!(r.got.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_of_public_addresses_only_does_not_reach_this_machine() {
    let r = receiver(vec![200]);
    let dir = temp("public");
    let (cfg, token) = project(&dir, r.port);
    let options = HostOptions {
        public_egress_only: true,
        ..Default::default()
    };
    let (_host, _rt, port) = serve(options, cfg.clone());
    place_order(port, &token, "D1");
    let row = outbox_row(&cfg, |r| r[4].is_some());
    assert!(
        row[3]
            .as_deref()
            .unwrap_or("")
            .contains("not a public address"),
        "{row:?}"
    );
    assert!(r.got.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_delivery_the_events_wait() {
    let r = receiver(vec![200]);
    let dir = temp("held");
    let (mut cfg, token) = project(&dir, r.port);
    cfg.outside.deliver = false;
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "E1");
    std::thread::sleep(Duration::from_secs(2));
    let row = outbox_row(&cfg, |_| true);
    assert_eq!(row[0].as_deref(), Some("0"));
    assert!(row[1].is_none());
    assert!(r.got.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plain SMTP server that accepts every message and keeps its data.
fn smtp_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader};
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut w) = s else { continue };
            let mut r = BufReader::new(w.try_clone().unwrap());
            let _ = w.write_all(b"220 test ESMTP\r\n");
            let mut line = String::new();
            let mut data: Option<String> = None;
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                if let Some(d) = data.as_mut() {
                    if line == ".\r\n" {
                        g.lock().unwrap().push(data.take().unwrap());
                        let _ = w.write_all(b"250 queued\r\n");
                    } else {
                        d.push_str(&line);
                    }
                } else {
                    let up = line.to_ascii_uppercase();
                    let answer: &[u8] = if up.starts_with("EHLO") {
                        b"250-test\r\n250 8BITMIME\r\n"
                    } else if up.starts_with("DATA") {
                        data = Some(String::new());
                        b"354 go on\r\n"
                    } else if up.starts_with("QUIT") {
                        let _ = w.write_all(b"221 bye\r\n");
                        break;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(answer);
                }
                line.clear();
            }
        }
    });
    (port, got)
}

#[test]
fn an_email_event_is_sent_by_its_smtp_connection() {
    let (smtp, got) = smtp_server();
    let dir = temp("smtp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"m\"\n\n[connections.mail]\nkind = \"smtp\"\nurl = \"smtp://127.0.0.1:{smtp}\"\ninsecure = true\nfrom = \"Shop <shop@example.com>\"\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("main.qd"),
        "entity Order:\n    number: Text\n    email: Email\n\nevent SendEmail(to: Email, subject: Text, body: Text)\n\non Order created as o:\n    emit SendEmail(to=o.email, subject=f\"Order {o.number}\", body=\"Thank you\")\n\noutbound SendEmail via mail\n",
    )
    .unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(&dir).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    ops::init(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        now(),
    )
    .unwrap();
    let (token, _) = auth::create(
        &mut Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now(),
        None,
    )
    .unwrap();
    let cfg = Config {
        outside: Outside {
            env: BTreeMap::new(),
            deliver: true,
        },
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
        limits: Limits::default(),
        engines: Default::default(),
    };
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    let body = json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": "M1", "email": "ann@example.com" } }] }).to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("POST /v1/tx HTTP/1.1\r\nHost: m.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: m1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let row = outbox_row(&cfg, |r| r[1].is_some() || r[4].is_some());
    assert!(row[1].is_some(), "{row:?}");
    let mail = got.lock().unwrap().clone();
    assert_eq!(mail.len(), 1);
    assert!(mail[0].contains("Subject: Order M1"), "{}", mail[0]);
    assert!(mail[0].contains("To: ann@example.com"), "{}", mail[0]);
    assert!(mail[0].contains("Thank you"), "{}", mail[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_compiler_keeps_secrets_out_of_the_code_and_variables_out_of_formulas() {
    let dir = temp("compile");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        "[project]\nname = \"c\"\n\n[vars]\nSHOP = { default = \"Acme\" }\nTOKEN = { secret = true }\n\n[connections.mail]\nkind = \"smtp\"\nurl = \"smtps://smtp.example.com\"\nfrom = \"a@example.com\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.qd"),
        "entity Order:\n    number: Text\n    note: Text?\n    label: Text = f\"{number} {env.SHOP}\"\n\n    action ship():\n        note = env.SHOP\n        emit Leak(token=env.TOKEN)\n        emit Mail(to=\"a@example.com\")\n\nevent Leak(token: Text)\nevent Mail(to: Email)\nevent Orphan(x: Text)\n\noutbound Mail via mail\noutbound Leak via nowhere\n\non Orphan delivered as d:\n    let x = d.event.x\n\non Mail failed as f:\n    let e = f.error\n    let w = f.event.nope\n",
    )
    .unwrap();
    let checked = quadra_analysis::check(&Project::load(&dir).unwrap());
    let errors: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == quadra_sema::Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    let has = |s: &str| errors.iter().any(|e| e.contains(s));
    assert!(
        has("TOKEN is a secret: the code cannot read it"),
        "{errors:?}"
    );
    assert!(has("`env.SHOP` is not allowed here"), "{errors:?}");
    assert!(has("`Mail` has no `subject`"), "{errors:?}");
    assert!(has("no connection `nowhere`"), "{errors:?}");
    // an outcome reaction: on an event that something delivers, with its fields
    assert!(has("`Orphan` has no `outbound`"), "{errors:?}");
    assert!(has("no field `nope`"), "{errors:?}");
    // the action may read the variable: no error on `note = env.SHOP`
    assert_eq!(
        errors.iter().filter(|e| e.contains("env.SHOP")).count(),
        1,
        "{errors:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_receiver_that_asks_to_wait_is_obeyed_and_refused_credentials_are_tried_again() {
    // 429 with Retry-After: 120 → not before two minutes
    let r = receiver(vec![429]);
    let dir = temp("later");
    let (cfg, token) = project(&dir, r.port);
    let (host, rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "F1");
    let row = outbox_row(&cfg, |r| r[0].as_deref() == Some("1"));
    let next =
        chrono::NaiveDateTime::parse_from_str(row[2].as_deref().unwrap(), "%Y-%m-%dT%H:%M:%S")
            .unwrap();
    let wait = next - chrono::Utc::now().naive_utc();
    assert!(wait > chrono::Duration::seconds(100), "{row:?}");
    assert!(row[4].is_none(), "{row:?}");
    drop((host, rt));
    let _ = std::fs::remove_dir_all(&dir);

    // 401: the credentials of the connection, to fix: tried again, not failed
    let r = receiver(vec![401]);
    let dir = temp("unauthorized");
    let (cfg, token) = project(&dir, r.port);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "G1");
    let row = outbox_row(&cfg, |r| r[0].as_deref() == Some("1"));
    assert!(row[4].is_none() && row[2].is_some(), "{row:?}");
    assert!(
        row[3].as_deref().unwrap_or("").contains("credentials"),
        "{row:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_attempt_is_in_the_log_with_its_answer() {
    let r = receiver(vec![503, 200]);
    let dir = temp("log");
    let (cfg, token) = project(&dir, r.port);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "H1");
    outbox_row(&cfg, |r| r[0].as_deref() == Some("1"));
    let mut c = Conn::open(&cfg.url).unwrap();
    let id: u64 = c
        .query("SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_outbox\"", &[])
        .unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    let log = quadra_store::deliveries::log(&mut c, id).unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].step, "attempt");
    assert_eq!(log[0].outcome, "pending");
    assert_eq!(log[0].status, Some(503));
    assert_eq!(log[0].connection.as_deref(), Some("hooks"));
    assert!(log[0].duration_ms.is_some());
    assert!(log[0].error.as_deref().unwrap_or("").contains("503"));
    // skipped by hand, with who and why; then made pending again
    let at = "2026-11-27T12:00:00";
    assert!(quadra_store::deliveries::skip(&mut c, &[id], at, "cli:ann", "").is_err());
    assert_eq!(
        quadra_store::deliveries::skip(&mut c, &[id], at, "cli:ann", "the order was cancelled")
            .unwrap(),
        1
    );
    assert_eq!(
        quadra_store::deliveries::retry(&mut c, &[id], at, "cli:ann").unwrap(),
        1
    );
    let log = quadra_store::deliveries::log(&mut c, id).unwrap();
    let steps: Vec<(&str, &str)> = log
        .iter()
        .map(|s| (s.step.as_str(), s.outcome.as_str()))
        .collect();
    assert_eq!(
        steps,
        vec![
            ("attempt", "pending"),
            ("skip", "skipped"),
            ("retry", "pending")
        ]
    );
    assert_eq!(log[1].reason.as_deref(), Some("the order was cancelled"));
    assert_eq!(log[1].by.as_deref(), Some("cli:ann"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A JSON request to the host; the status and the body.
fn call(port: u16, token: &str, method: &str, path: &str, body: Option<J>) -> (u16, J) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    static KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let req = format!("{method} {path} HTTP/1.1\r\nHost: d.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: call-{}-{key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", std::process::id(), body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, serde_json::from_str(rest).unwrap_or(J::Null))
}

#[test]
fn an_integration_takes_the_events_no_outbound_delivers_and_settles_them() {
    // the same project, without its `outbound`: an integration delivers OrderPlaced
    let dir = temp("pull");
    let (cfg, token) = project(&dir, 9);
    let code = std::fs::read_to_string(dir.join("main.qd")).unwrap();
    std::fs::write(
        dir.join("main.qd"),
        code.replace("outbound OrderPlaced via hooks path \"/orders\"\n", ""),
    )
    .unwrap();
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "P1");
    place_order(port, &token, "P2");
    // taken, leased: a second take within the lease gets nothing
    let (s, v) = call(
        port,
        &token,
        "GET",
        "/v1/outbox?events=OrderPlaced&limit=10&lease=30",
        None,
    );
    assert_eq!(s, 200, "{v}");
    let events = v["events"].as_array().unwrap().clone();
    assert_eq!(events.len(), 2, "{v}");
    assert_eq!(events[0]["data"]["number"], "P1");
    assert!(events[0]["key"].as_str().unwrap().starts_with("quadra-"));
    let (_, again) = call(port, &token, "GET", "/v1/outbox", None);
    assert!(again["events"].as_array().unwrap().is_empty(), "{again}");
    // one delivered, one refused for now: the refused one comes back after its wait
    let (id1, id2) = (
        events[0]["id"].as_u64().unwrap(),
        events[1]["id"].as_u64().unwrap(),
    );
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/outbox/ack",
        Some(json!({ "ids": [id1] })),
    );
    assert_eq!((s, v["delivered"].clone()), (200, json!([id1])));
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/outbox/nack",
        Some(json!({ "id": id2, "error": "the ERP is down", "retry_after": 0 })),
    );
    assert_eq!(s, 200, "{v}");
    let (_, v) = call(port, &token, "GET", "/v1/outbox", None);
    let back = v["events"].as_array().unwrap();
    assert_eq!(back.len(), 1, "{v}");
    assert_eq!(
        (back[0]["id"].as_u64(), back[0]["attempts"].as_u64()),
        (Some(id2), Some(1))
    );
    // an event already delivered is not settled again
    let (s, _) = call(
        port,
        &token,
        "POST",
        "/v1/outbox/nack",
        Some(json!({ "id": id1, "error": "x" })),
    );
    assert_eq!(s, 404);
    // the log says who settled them
    let log = quadra_store::deliveries::log(&mut Conn::open(&cfg.url).unwrap(), id1).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, "delivered");
    assert!(
        log[0]
            .connection
            .as_deref()
            .unwrap_or("")
            .starts_with("pull:user:"),
        "{log:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const REACTING: &str = "entity Order:\n    number: Text\n    note: Text?\n\nevent OrderPlaced(order: Order, number: Text)\n\non Order created as o:\n    emit OrderPlaced(order=o, number=o.number)\n\noutbound OrderPlaced via hooks path \"/orders\"\n\non OrderPlaced delivered as d:\n    d.event.order.note = f\"sent: {d.status ?? 0} after {d.attempts} attempt(s)\"\n\non OrderPlaced failed as f:\n    f.event.order.note = f\"not sent: {f.error}\"\n";

fn note_of(cfg: &Config, number: &str) -> Option<String> {
    let rows = Conn::open(&cfg.url)
        .unwrap()
        .query(
            "SELECT \"note\" FROM \"order\" WHERE \"number\" = ?text",
            &[quadra_store::conn::Sql::Text(number.to_string())],
        )
        .unwrap_or_default();
    rows.into_iter().next().and_then(|r| r[0].clone())
}

#[test]
fn the_outcome_of_a_delivery_is_a_transaction_the_code_reacts_to() {
    // delivered: the reaction writes on the order, with the cause of the delivery
    let r = receiver(vec![201]);
    let dir = temp("react-ok");
    let (cfg, token) = project_with(&dir, r.port, REACTING);
    let (host, rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "R1");
    let start = Instant::now();
    while note_of(&cfg, "R1").is_none() && start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        note_of(&cfg, "R1").as_deref(),
        Some("sent: 201 after 1 attempt(s)")
    );
    let cause: String = Conn::open(&cfg.url)
        .unwrap()
        .query(
            "SELECT \"cause\" FROM \"_quadra_commits\" ORDER BY \"tx\" DESC LIMIT 1",
            &[],
        )
        .unwrap()[0][0]
        .clone()
        .unwrap();
    assert!(
        cause.contains("outbound:OrderPlaced#") && cause.contains("delivered"),
        "{cause}"
    );
    drop((host, rt));
    let _ = std::fs::remove_dir_all(&dir);

    // refused for good: the other reaction, with the error
    let r = receiver(vec![422]);
    let dir = temp("react-failed");
    let (cfg, token) = project_with(&dir, r.port, REACTING);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place_order(port, &token, "R2");
    let start = Instant::now();
    while note_of(&cfg, "R2").is_none() && start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let note = note_of(&cfg, "R2").unwrap_or_default();
    assert!(
        note.starts_with("not sent:") && note.contains("422"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const SHAPED: &str = "entity Order:\n    number: Text\n    weight: Decimal\n    note: Text?\n\nrecord ErpShipment from json:\n    order: Text at \"order.number\"\n    weight: Decimal at \"parcel.weight_kg\"\n\nrecord ErpAck from json:\n    tracking: Text\n\nevent OrderPlaced(order: Order)\n\non Order created as o:\n    emit OrderPlaced(order=o)\n\noutbound OrderPlaced via hooks as e:\n    reply ErpAck\n    let o = e.order\n    put(f\"/shipments/{o.number}\", ErpShipment(order=o.number, weight=o.weight))\n\non OrderPlaced delivered as d:\n    d.event.order.note = d.reply?.tracking ?? \"no tracking\"\n";

#[test]
fn an_outbound_block_builds_the_request_in_the_shape_of_the_receiver() {
    let r = receiver(vec![200]);
    let dir = temp("shaped");
    let (cfg, token) = project_with(&dir, r.port, SHAPED);
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    let body = json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": "S1", "weight": 2.5 } }] }).to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("POST /v1/tx HTTP/1.1\r\nHost: d.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: s1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    outbox_row(&cfg, |r| r[1].is_some());
    let got = r.got.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    let (h, body) = &got[0];
    assert_eq!(h[":method"], "PUT");
    assert_eq!(h[":path"], "/shipments/S1");
    assert_eq!(h["content-type"], "application/json");
    assert_eq!(
        body,
        "{\"order\":{\"number\":\"S1\"},\"parcel\":{\"weight_kg\":2.5}}"
    );
    assert!(h.contains_key("idempotency-key"));
    // the answer, read as the `reply` record, reached the reaction
    let start = Instant::now();
    while note_of(&cfg, "S1").is_none() && start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(note_of(&cfg, "S1").as_deref(), Some("TRK-42"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_block_reads_the_data_as_it_was_when_the_event_was_emitted() {
    let r = receiver(vec![200]);
    let dir = temp("asof");
    let (mut cfg, token) = project_with(&dir, r.port, SHAPED);
    // made and emitted, then changed, while nothing is delivered
    cfg.outside.deliver = false;
    {
        let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
        let (s, v) = call(
            port,
            &token,
            "POST",
            "/v1/tx",
            Some(
                json!({ "ops": [{ "op": "create", "entity": "Order", "ref": "o", "values": { "number": "A1", "weight": 1 } }] }),
            ),
        );
        assert_eq!(s, 200, "{v}");
        let id = v["created"]["o"].as_u64().unwrap();
        let base = v["base"].clone();
        let (s, v) = call(
            port,
            &token,
            "POST",
            "/v1/tx",
            Some(
                json!({ "base": base, "ops": [{ "op": "update", "entity": "Order", "id": id, "values": { "number": "A2", "weight": 9 } }] }),
            ),
        );
        assert_eq!(s, 200, "{v}");
    }
    // delivered later: what the block reads is the order as it was at the emit
    cfg.outside.deliver = true;
    let (_host, _rt, port) = serve(HostOptions::default(), cfg.clone());
    // (the test's host starts the app at its first request; `quadra serve` at once)
    call(port, &token, "GET", "/v1/me", None);
    outbox_row(&cfg, |r| r[1].is_some());
    let got = r.got.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0[":path"], "/shipments/A1");
    assert_eq!(
        got[0].1,
        "{\"order\":{\"number\":\"A1\"},\"parcel\":{\"weight_kg\":1}}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The project of `REACTING`'s orders with an outbound whose block holds `policy` lines.
fn with_policy(policy: &str) -> String {
    format!("entity Order:\n    number: Text\n    customer: Text\n    note: Text?\n\nevent OrderPlaced(order: Order, customer: Text)\n\non Order created as o:\n    emit OrderPlaced(order=o, customer=o.customer)\n\noutbound OrderPlaced via hooks path \"/orders\" as e:\n{}\n", policy.lines().map(|l| format!("    {l}\n")).collect::<String>())
}

fn create(port: u16, token: &str, number: &str, customer: &str, note: Option<&str>) {
    let mut values = json!({ "number": number, "customer": customer });
    if let Some(n) = note {
        values["note"] = json!(n);
    }
    let (s, v) = call(
        port,
        token,
        "POST",
        "/v1/tx",
        Some(json!({ "ops": [{ "op": "create", "entity": "Order", "values": values }] })),
    );
    assert_eq!(s, 200, "{v}");
}

/// The states of the outbox, in the order of the events, once `ready` says so (within 15 s).
fn states(cfg: &Config, ready: impl Fn(&[String]) -> bool) -> Vec<String> {
    let start = Instant::now();
    loop {
        let s: Vec<String> = Conn::open(&cfg.url)
            .unwrap()
            .query(
                "SELECT \"state\" FROM \"_quadra_outbox\" ORDER BY \"id\"",
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect();
        if ready(&s) || start.elapsed() > Duration::from_secs(15) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_policy_of_an_outbound_decides_how_long_and_whether_it_goes() {
    // retry 2 times: failed after the second attempt
    let r = receiver(vec![503]);
    let dir = temp("p-times");
    let (cfg, token) = project_with(
        &dir,
        r.port,
        &with_policy("retry 2 times\nbackoff 1 seconds .. 1 seconds"),
    );
    let (h, rt, port) = serve(HostOptions::default(), cfg.clone());
    create(port, &token, "T1", "ann", None);
    assert_eq!(states(&cfg, |s| s == ["failed"]), ["failed"]);
    assert_eq!(r.got.lock().unwrap().len(), 2);
    drop((h, rt));
    let _ = std::fs::remove_dir_all(&dir);

    // expires after: not delivered in time, expired
    let r = receiver(vec![503]);
    let dir = temp("p-expires");
    let (cfg, token) = project_with(
        &dir,
        r.port,
        &with_policy("expires after 2 seconds\nbackoff 1 seconds .. 1 seconds"),
    );
    let (h, rt, port) = serve(HostOptions::default(), cfg.clone());
    create(port, &token, "X1", "ann", None);
    assert_eq!(states(&cfg, |s| s == ["expired"]), ["expired"]);
    drop((h, rt));
    let _ = std::fs::remove_dir_all(&dir);

    // unless, on the data as it is: skipped, never sent; delay: not yet
    let r = receiver(vec![200]);
    let dir = temp("p-unless");
    let (cfg, token) = project_with(
        &dir,
        r.port,
        &with_policy("unless e.order.note == \"cancelled\"\ndelay 0 seconds"),
    );
    let (h, rt, port) = serve(HostOptions::default(), cfg.clone());
    create(port, &token, "U1", "ann", Some("cancelled"));
    create(port, &token, "U2", "bob", None);
    assert_eq!(
        states(&cfg, |s| s == ["skipped", "delivered"]),
        ["skipped", "delivered"]
    );
    assert_eq!(r.got.lock().unwrap().len(), 1);
    drop((h, rt));
    let _ = std::fs::remove_dir_all(&dir);

    let r = receiver(vec![200]);
    let dir = temp("p-delay");
    let (cfg, token) = project_with(
        &dir,
        r.port,
        &with_policy("delay 1 hours\nkey f\"order-{e.order.number}\""),
    );
    let (h, rt, port) = serve(HostOptions::default(), cfg.clone());
    create(port, &token, "D1", "ann", None);
    std::thread::sleep(Duration::from_secs(2));
    let row = outbox_row(&cfg, |r| r[2].is_some());
    assert_eq!(row[0].as_deref(), Some("0"), "not tried: {row:?}");
    assert!(r.got.lock().unwrap().is_empty());
    drop((h, rt));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ordered_by_sends_one_at_a_time_per_key_and_latest_by_only_the_newest() {
    // ordered by the customer: ann's second order waits for her first, bob's goes
    let r = receiver(vec![503]);
    let dir = temp("p-ordered");
    let (cfg, token) = project_with(&dir, r.port, &with_policy("ordered by e.customer\nbackoff 60 seconds .. 60 seconds\nkey f\"order-{e.order.number}\""));
    let (h, rt, port) = serve(HostOptions::default(), cfg.clone());
    create(port, &token, "A1", "ann", None);
    create(port, &token, "A2", "ann", None);
    create(port, &token, "B1", "bob", None);
    std::thread::sleep(Duration::from_secs(3));
    let paths: Vec<String> = r
        .got
        .lock()
        .unwrap()
        .iter()
        .map(|(h, _)| h["idempotency-key"].clone())
        .collect();
    assert!(
        paths.contains(&"order-A1".to_string()) && paths.contains(&"order-B1".to_string()),
        "{paths:?}"
    );
    assert!(
        !paths.contains(&"order-A2".to_string()),
        "A2 waits for A1: {paths:?}"
    );
    drop((h, rt));
    let _ = std::fs::remove_dir_all(&dir);

    // latest by the customer: three events while nothing is delivered, only the last goes
    let r = receiver(vec![200]);
    let dir = temp("p-latest");
    let (mut cfg, token) = project_with(&dir, r.port, &with_policy("latest by e.customer"));
    cfg.outside.deliver = false;
    {
        let (_h, _rt, port) = serve(HostOptions::default(), cfg.clone());
        for n in ["L1", "L2", "L3"] {
            create(port, &token, n, "ann", None);
        }
    }
    cfg.outside.deliver = true;
    let (_h, _rt, port) = serve(HostOptions::default(), cfg.clone());
    call(port, &token, "GET", "/v1/me", None);
    assert_eq!(
        states(&cfg, |s| s.last().map(String::as_str) == Some("delivered")),
        ["superseded", "superseded", "delivered"]
    );
    assert_eq!(r.got.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
