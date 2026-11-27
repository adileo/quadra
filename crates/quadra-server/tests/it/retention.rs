//! Retention over HTTP: a record its retention took away answers `410`; `erase` needs its
//! permission, is refused while an invoice keeps the customer, and empties the personal
//! fields; the export of a subject is given and is in the chain; the database verifies.

use crate::deliver::serve;
use quadra_sema::Project;
use quadra_server::{Config, HostOptions, Limits, Outside};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const CODE: &str = r#"
entity Customer:
    name: Text? personal
    email: Email? personal
    city: Text?

entity Invoice:
    retain 10 years after date
    customer: Customer
    date: Date
    total: Money<EUR>

entity Session:
    expire 2 days after created_at
    token: Text

role Clerk:
    read, create, edit Customer, Invoice, Session

role Privacy:
    read Customer
    read history Customer
    erase Customer

seed:
    User(login="clara", roles={Clerk})
    User(login="dora", roles={Privacy})

fixture demo:
    let anna = Customer(name="Anna Rossi", email="anna@example.com", city="Milano")
    Invoice(customer=anna, date=today(), total=100.00 EUR)
    Customer(name="Bruno Bianchi", email="bruno@example.com", city="Torino")
    Session(token="abc")
"#;

fn http(port: u16, method: &str, path: &str, bearer: &str, body: Option<&J>) -> (u16, J) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
    // (a chunked body: the JSON is between the chunk sizes)
    let start = body.find(['{', '[']).unwrap_or(0);
    let end = body.rfind(['}', ']']).map(|e| e + 1).unwrap_or(body.len());
    (
        status,
        serde_json::from_str(&body[start..end]).unwrap_or(J::Null),
    )
}

fn id_where(i: &mut quadra_runtime::Interp, ent: &str, field: &str, value: &str) -> u64 {
    i.ids(ent, None)
        .into_iter()
        .find(|id| {
            i.store
                .read(*id, field, None)
                .is_some_and(|v| v.to_string() == value)
        })
        .unwrap()
}

#[test]
fn retention_and_erasure_over_http() {
    let dir = std::env::temp_dir().join(format!("quadra-retention-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"r\"\n").unwrap();
    std::fs::write(dir.join("main.qd"), CODE).unwrap();
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
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let now = chrono::Utc::now().naive_utc();
    let then = now - chrono::Duration::days(10);
    ops::init(&url, m, r, then).unwrap();
    ops::fixture(&url, m, r, "demo", then).unwrap();
    // the session is past its two days: the sweep takes it away
    assert!(ops::sweep(&url, m, r, now).unwrap().is_some());
    let mut db = quadra_store::Database::open(&url, m, r).unwrap();
    let mut i = ops::loaded(&mut db, m, now).unwrap();
    let clara = id_where(&mut i, "User", "login", "clara");
    let dora = id_where(&mut i, "User", "login", "dora");
    let anna = id_where(&mut i, "Customer", "name", "Anna Rossi");
    let bruno = id_where(&mut i, "Customer", "name", "Bruno Bianchi");
    assert!(i.ids("Session", None).is_empty());
    drop(i);
    let mut conn = Conn::open(&url).unwrap();
    let session: u64 = conn
        .query("SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_erased\"", &[])
        .unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    let (ct, _) = auth::create(&mut conn, "token", clara, "t", false, now, None).unwrap();
    let (dt, _) = auth::create(&mut conn, "token", dora, "t", false, now, None).unwrap();
    drop(conn);
    let cfg = Config {
        outside: Outside {
            env: Default::default(),
            deliver: false,
        },
        root: dir.clone(),
        url: url.clone(),
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
    let (_host, _rt, port) = serve(HostOptions::default(), cfg);

    // the session: gone, with when and why
    let (s, b) = http(
        port,
        "GET",
        &format!("/v1/records/Session/{session}"),
        &ct,
        None,
    );
    assert_eq!(s, 410, "{b}");
    let msg = b["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("was erased on") && msg.contains("2 days after created_at"),
        "{msg}"
    );

    // erasing needs the `erase` permission
    let reason = json!({ "reason": "he asked on the phone" });
    let (s, b) = http(
        port,
        "POST",
        &format!("/v1/records/Customer/{bruno}/erase"),
        &ct,
        Some(&reason),
    );
    assert_eq!(s, 403, "{b}");
    // an invoice keeps Anna for ten years
    let (s, b) = http(
        port,
        "POST",
        &format!("/v1/records/Customer/{anna}/erase"),
        &dt,
        Some(&reason),
    );
    assert_eq!(s, 422, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot be erased before"),
        "{b}"
    );
    // Bruno's personal fields emptied, the rest kept
    let (s, b) = http(
        port,
        "POST",
        &format!("/v1/records/Customer/{bruno}/erase"),
        &dt,
        Some(&reason),
    );
    assert_eq!(s, 200, "{b}");
    let (s, b) = http(
        port,
        "GET",
        &format!("/v1/records/Customer/{bruno}"),
        &ct,
        None,
    );
    assert_eq!(s, 200, "{b}");
    let text = b.to_string();
    assert!(
        !text.contains("Bruno") && !text.contains("bruno@"),
        "{text}"
    );
    assert!(text.contains("Torino"), "{text}");
    // the export: the record, the history of its personal fields (the values erased)
    let (s, b) = http(
        port,
        "GET",
        &format!("/v1/records/Customer/{bruno}/export"),
        &dt,
        None,
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["subject"]["id"], json!(bruno));
    assert!(!b["personal_history"].as_array().unwrap().is_empty());
    assert!(!b.to_string().contains("Bruno"), "{b}");
    drop(_host);

    // the erasure and the export are in the chain, which verifies
    let mut conn = Conn::open(&url).unwrap();
    let causes: Vec<String> = conn
        .query(
            "SELECT \"cause\" FROM \"_quadra_commits\" ORDER BY \"tx\"",
            &[],
        )
        .unwrap()
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect();
    assert!(
        causes.iter().any(|c| c.starts_with("expire: Session#")),
        "{causes:?}"
    );
    assert!(
        causes.iter().any(|c| c.starts_with("export: Customer#")),
        "{causes:?}"
    );
    let erasures = conn
        .query(
            "SELECT \"reason\" FROM \"_quadra_erasures\" WHERE \"record_id\" = ?bigint",
            &[quadra_store::conn::Sql::Int(bruno as i64)],
        )
        .unwrap();
    assert_eq!(erasures[0][0].as_deref(), Some("he asked on the phone"));
    drop(conn);
    let (problems, _) = ops::verify(&url, m, r, now).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let dump = {
        let mut c = Conn::open(&url).unwrap();
        let mut out = String::new();
        for t in c
            .query("SELECT name FROM sqlite_master WHERE type = 'table'", &[])
            .unwrap()
        {
            let t = t[0].clone().unwrap();
            for row in c.query(&format!("SELECT * FROM \"{t}\""), &[]).unwrap() {
                out.push_str(&format!("{row:?}\n"));
            }
        }
        out
    };
    assert!(!dump.contains("Bruno Bianchi") && !dump.contains("bruno@example.com"));
    assert!(!dump.contains("\"abc\""), "the session's token is gone too");
    let _ = std::fs::remove_dir_all(&dir);
}
