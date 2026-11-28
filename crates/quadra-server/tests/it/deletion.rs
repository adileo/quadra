//! Deletion over HTTP: `DELETE` needs the `delete` permission and sends a record to its trash
//! (`410` with until when, listed by `GET /v1/trash`), `POST …/restore` gives it back with its
//! id; an entity without a trash deletes for good.

use crate::deliver::serve;
use quadra_sema::Project;
use quadra_server::{Config, HostOptions, Limits, Outside};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const CODE: &str = r#"
entity Page:
    deletable, trash 30 days
    history last 5
    slug: Text unique
    title: Text

entity Session:
    deletable
    history none
    token: Text

role Reader:
    read Page, Session

role Editor:
    read, create, edit, delete Page, Session

seed:
    User(login="rita", roles={Reader})
    User(login="ed", roles={Editor})

fixture demo:
    Page(slug="home", title="Home")
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
fn delete_trash_and_restore_over_http() {
    let dir = std::env::temp_dir().join(format!("quadra-deletion-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"d\"\n").unwrap();
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
    ops::init(&url, m, r, now).unwrap();
    ops::fixture(&url, m, r, "demo", now).unwrap();
    let mut db = quadra_store::Database::open(&url, m, r).unwrap();
    let mut i = ops::loaded(&mut db, m, now).unwrap();
    let id_of = |i: &mut quadra_runtime::Interp, ent: &str, field: &str, value: &str| -> u64 {
        i.ids(ent, None)
            .into_iter()
            .find(|id| {
                i.store
                    .read(*id, field, None)
                    .is_some_and(|v| v.to_string() == value)
            })
            .unwrap()
    };
    let rita = id_of(&mut i, "User", "login", "rita");
    let ed = id_of(&mut i, "User", "login", "ed");
    let page = id_of(&mut i, "Page", "slug", "home");
    let session = id_of(&mut i, "Session", "token", "abc");
    drop(i);
    let mut conn = Conn::open(&url).unwrap();
    let (rt, _) = auth::create(&mut conn, "token", rita, "t", false, now, None).unwrap();
    let (et, _) = auth::create(&mut conn, "token", ed, "t", false, now, None).unwrap();
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
    let page_path = format!("/v1/records/Page/{page}");

    // a reader may not delete
    let (s, b) = http(port, "DELETE", &page_path, &rt, None);
    assert_eq!(s, 403, "{b}");
    // to the trash: gone, with until when
    let (s, b) = http(port, "DELETE", &page_path, &et, None);
    assert_eq!(s, 200, "{b}");
    let (s, b) = http(port, "GET", &page_path, &et, None);
    assert_eq!(s, 410, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap_or("")
            .contains("is in the trash since"),
        "{b}"
    );
    let (s, b) = http(port, "GET", "/v1/trash", &et, None);
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["records"][0]["id"], json!(page), "{b}");
    assert_eq!(b["records"][0]["values"]["slug"], json!("home"), "{b}");
    let (_, b) = http(port, "GET", "/v1/trash", &rt, None);
    assert_eq!(b["records"], json!([]), "a reader sees no trash: {b}");
    // back, with its id
    let (s, b) = http(
        port,
        "POST",
        &format!("{page_path}/restore"),
        &et,
        Some(&json!({})),
    );
    assert_eq!(s, 200, "{b}");
    let (s, b) = http(port, "GET", &page_path, &rt, None);
    assert_eq!(s, 200, "{b}");
    assert!(b.to_string().contains("Home"), "{b}");
    // a session goes for good
    let sp = format!("/v1/records/Session/{session}");
    let (s, b) = http(port, "DELETE", &sp, &et, None);
    assert_eq!(s, 200, "{b}");
    // (`history none`: nothing is left of it, not even when it was deleted)
    let (s, b) = http(port, "GET", &sp, &et, None);
    assert_eq!(s, 404, "{b}");
    drop(_host);
    let (problems, _) = ops::verify(&url, m, r, chrono::Utc::now().naive_utc()).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
