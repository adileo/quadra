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
            let headers: HashMap<String, String> = head
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
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
            let _ = s.write_all(
                format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: 2\r\nConnection: close\r\n\r\nno")
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
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"d\"\n\n[vars]\nHOOK_SECRET = {{ secret = true }}\n\n[connections.hooks]\nkind = \"http\"\nurl = \"http://127.0.0.1:{port}\"\ninsecure = true\nsigning_secret = {{ var = \"HOOK_SECRET\" }}\ntimeout = \"3s\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), CODE).unwrap();
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
        "entity Order:\n    number: Text\n    note: Text?\n    label: Text = f\"{number} {env.SHOP}\"\n\n    action ship():\n        note = env.SHOP\n        emit Leak(token=env.TOKEN)\n        emit Mail(to=\"a@example.com\")\n\nevent Leak(token: Text)\nevent Mail(to: Email)\n\noutbound Mail via mail\noutbound Leak via nowhere\n",
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
