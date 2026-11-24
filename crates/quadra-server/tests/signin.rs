//! Sign-in with a code sent by email, on a host of two tenants with a fake sender: the code
//! reaches the user's address, works once and only for its request, a session of one tenant
//! means nothing on another, a user who does not exist gets the same answer and no email, and
//! only an admin invites.

use quadra_sema::Project;
use quadra_server::{Config, Host, HostOptions, Limits, SignIn, SignInMail, TenantResolver, TenantSpec};
use quadra_store::ops;
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CODE: &str = r#"
role Admin:
    all *

role Clerk:
    read Note

extend User:
    email: Email?

entity Note:
    text: Text

seed:
    User(login="ann@example.com", roles={Admin})
    User(login="bob", email="bob@example.com", roles={Clerk})
    User(login="nomail")
"#;

fn project(dir: &Path) -> Config {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"t\"\n").unwrap();
    std::fs::write(dir.join("main.qd"), CODE).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    assert!(checked.model.is_some(), "{:?}", checked.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, chrono::Utc::now().naive_utc()).unwrap();
    Config { root: dir.to_path_buf(), url, port: 0, dev_login: false, secure_cookies: false, cache_mb: 16, prewarm_days: None, web: None, sweep_every: 3600, editors: vec![], watch: false, limits: Limits::default(), engines: Default::default() }
}

struct Map(HashMap<String, TenantSpec>);

impl TenantResolver for Map {
    fn resolve(&self, host: &str) -> Option<TenantSpec> {
        self.0.get(host).cloned()
    }
}

#[derive(Default)]
struct Outbox(Mutex<Vec<(String, String, SignInMail)>>);

impl SignIn for Outbox {
    fn send(&self, tenant: &str, host: &str, mail: SignInMail) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        self.0.lock().unwrap().push((tenant.into(), host.into(), mail));
        Box::pin(async { Ok(()) })
    }
}

/// (status, body, the session cookie it set)
fn http(port: u16, host: &str, method: &str, path: &str, body: Option<J>, session: Option<&str>) -> (u16, J, Option<String>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let auth = session.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let cookie = head.lines().find_map(|l| l.strip_prefix("set-cookie: quadra_session=").or_else(|| l.strip_prefix("Set-Cookie: quadra_session="))).map(|c| c.split(';').next().unwrap().to_string());
    (status, serde_json::from_str(rest).unwrap_or(J::Null), cookie)
}

#[test]
fn sign_in_by_email_code_on_two_tenants() {
    let base = std::env::temp_dir().join(format!("quadra-signin-{}", std::process::id()));
    let a = project(&base.join("a"));
    let mut b = project(&base.join("b"));
    b.limits.max_users = Some(3);
    let outbox = Arc::new(Outbox::default());
    let resolver = Map([("a.test".to_string(), TenantSpec { id: "ta".into(), config: a }), ("b.test".to_string(), TenantSpec { id: "tb".into(), config: b })].into_iter().collect());
    let host = Host::new(HostOptions { sign_in: Some(outbox.clone()), ..Default::default() }, Arc::new(resolver));
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
    let last = |to: &str| -> String {
        outbox
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(_, _, m)| match m {
                SignInMail::Code { to: t, code } if t == to => Some(code.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no code to {to}"))
    };

    let (s, methods, _) = http(port, "a.test", "GET", "/v1/auth", None, None);
    assert_eq!((s, methods["code"].as_bool(), methods["dev_login"].as_bool()), (200, Some(true), Some(false)));

    // a code to the login that is an address; wrong, then right; then used
    let (s, r, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "Ann@Example.com" })), None);
    assert_eq!(s, 200, "{r}");
    assert!(r.get("_send").is_none(), "what was sent stays on the server");
    let id = r["login_id"].as_str().unwrap().to_string();
    let code = last("ann@example.com");
    let (s, r, _) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": "0000-0000" })), None);
    assert_eq!((s, r["code"].as_str()), (400, Some("wrong")), "{r}");
    let (s, r, cookie) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": code.to_lowercase() })), None);
    assert_eq!(s, 200, "{r}");
    let session = cookie.expect("a session cookie");
    let (s, me, _) = http(port, "a.test", "GET", "/v1/me", None, Some(&session));
    assert_eq!((s, me["user"].as_str()), (200, Some("ann@example.com")));
    let (_, r, _) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": code })), None);
    assert_eq!(r["code"].as_str(), Some("used"));

    // the session of tenant a means nothing on tenant b
    let (s, _, _) = http(port, "b.test", "GET", "/v1/me", None, Some(&session));
    assert_eq!(s, 401);
    // nor does a code of a's request on b
    let (_, r, _) = http(port, "b.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": code })), None);
    assert_eq!(r["code"].as_str(), Some("unknown"));

    // a user with an email field: the code goes there; by login or by address
    http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "bob" })), None);
    assert!(!last("bob@example.com").is_empty());
    let n = outbox.0.lock().unwrap().len();
    // nobody, or a user without an address: the same answer, and nothing sent
    let (s1, r1, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "nobody@example.com" })), None);
    let (s2, r2, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "nomail" })), None);
    assert_eq!((s1, s2), (200, 200));
    assert!(r1["login_id"].is_string() && r2["login_id"].is_string());
    assert_eq!(outbox.0.lock().unwrap().len(), n, "no email for them");

    // five wrong attempts lock a code
    let (_, r, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "bob@example.com" })), None);
    let id = r["login_id"].as_str().unwrap().to_string();
    let good = last("bob@example.com");
    for _ in 0..5 {
        http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": "0000-0000" })), None);
    }
    let (_, r, _) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": good })), None);
    assert_eq!(r["code"].as_str(), Some("locked"));

    // an admin invites; a clerk cannot
    let (_, q, _) = http(port, "a.test", "POST", "/v1/query", Some(json!({ "entity": "User", "fields": ["login"], "where": "login == \"bob\"" })), Some(&session));
    let bob_id = q["rows"][0]["id"].as_u64().unwrap_or_else(|| panic!("{q}"));
    let (s, r, _) = http(port, "a.test", "POST", "/v1/auth/invite", Some(json!({ "user_id": bob_id })), Some(&session));
    assert_eq!(s, 200, "{r}");
    assert!(outbox.0.lock().unwrap().iter().any(|(t, h, m)| t == "ta" && h == "a.test" && matches!(m, SignInMail::Invite { to, login } if to == "bob@example.com" && login == "bob")));
    // bob (a clerk) signs in, and cannot invite
    let (_, r, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "bob" })), None);
    let id = r["login_id"].as_str().unwrap().to_string();
    let (_, _, bob) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": last("bob@example.com") })), None);
    let (s, _, _) = http(port, "a.test", "POST", "/v1/auth/invite", Some(json!({ "user_id": bob_id })), bob.as_deref());
    assert_eq!(s, 403);
    // every email named the tenant and the address it was asked at
    assert!(outbox.0.lock().unwrap().iter().all(|(t, h, _)| t == "ta" && h == "a.test"));

    // the host (a console) lists the roles and the users, and adds a user who is invited
    let people = rt.block_on(host.people("a.test")).unwrap();
    assert_eq!(people["roles"], json!(["Admin", "Clerk"]), "{people}");
    assert_eq!(people["total"], json!(3));
    let bob_row = people["users"].as_array().unwrap().iter().find(|u| u["login"] == "bob").unwrap().clone();
    assert_eq!((bob_row["email"].as_str(), &bob_row["roles"]), (Some("bob@example.com"), &json!(["Clerk"])));
    let added = rt.block_on(host.add_user("a.test", " Carl@Example.com ", vec!["Clerk".into()])).unwrap();
    assert_eq!((added["login"].as_str(), added["created"].as_bool(), added["invited"].as_bool()), (Some("carl@example.com"), Some(true), Some(true)), "{added}");
    assert!(added.get("_send").is_none());
    assert!(matches!(&outbox.0.lock().unwrap().last().unwrap().2, SignInMail::Invite { to, .. } if to == "carl@example.com"));
    // carl signs in with a code and has his role: he reads notes, he cannot invite
    let (_, r, _) = http(port, "a.test", "POST", "/v1/auth/code", Some(json!({ "login": "carl@example.com" })), None);
    let id = r["login_id"].as_str().unwrap().to_string();
    let (s, r, carl) = http(port, "a.test", "POST", "/v1/auth/code/verify", Some(json!({ "login_id": id, "code": last("carl@example.com") })), None);
    assert_eq!(s, 200, "{r}");
    let (s, _, _) = http(port, "a.test", "POST", "/v1/query", Some(json!({ "entity": "Note", "fields": ["text"] })), carl.as_deref());
    assert_eq!(s, 200);
    let (s, _, _) = http(port, "a.test", "POST", "/v1/auth/invite", Some(json!({ "user_id": bob_id })), carl.as_deref());
    assert_eq!(s, 403);
    // again: the same user, invited again; by the email field too
    let again = rt.block_on(host.add_user("a.test", "carl@example.com", vec!["Admin".into()])).unwrap();
    assert_eq!((again["created"].as_bool(), again["id"].clone()), (Some(false), added["id"].clone()));
    let bob_again = rt.block_on(host.add_user("a.test", "bob@example.com", vec![])).unwrap();
    assert_eq!((bob_again["login"].as_str(), bob_again["created"].as_bool()), (Some("bob"), Some(false)));
    assert_eq!(rt.block_on(host.people("a.test")).unwrap()["total"], json!(4));
    // what is refused
    assert_eq!(rt.block_on(host.add_user("a.test", "dana@example.com", vec!["Boss".into()])).unwrap_err().0, 400);
    assert_eq!(rt.block_on(host.add_user("a.test", "not an address", vec![])).unwrap_err().0, 400);
    assert_eq!(rt.block_on(host.add_user("nowhere.test", "dana@example.com", vec![])).unwrap_err().0, 404);
    // the other tenant has none of them, and it is at its limit of users
    assert_eq!(rt.block_on(host.people("b.test")).unwrap()["total"], json!(3));
    let (s, m) = rt.block_on(host.add_user("b.test", "dana@example.com", vec![])).unwrap_err();
    assert_eq!(s, 422, "{m}");
    assert!(m.contains("too many users"), "{m}");
    assert_eq!(rt.block_on(host.user_count("b.test")).unwrap(), Some(3));

    // usage: what each tenant's requests used, taken once; the same request counts the same
    let q = json!({ "entity": "User", "fields": ["login"] });
    // once to warm the caches: a request served from the cache reads fewer rows
    http(port, "a.test", "POST", "/v1/query", Some(q.clone()), Some(&session));
    host.take_usage();
    http(port, "a.test", "POST", "/v1/query", Some(q.clone()), Some(&session));
    let one: HashMap<String, quadra_server::Usage> = host.take_usage().into_iter().collect();
    let a1 = one["ta"];
    assert!(!one.contains_key("tb"), "b did nothing: {one:?}");
    assert_eq!(a1.requests, 1, "{a1:?}");
    assert!(a1.rows_read > 0, "{a1:?}");
    assert!(host.take_usage().is_empty(), "taken once");
    http(port, "a.test", "POST", "/v1/query", Some(q), Some(&session));
    let a2 = host.take_usage().into_iter().find(|(t, _)| t == "ta").unwrap().1;
    assert_eq!((a2.requests, a2.steps, a2.rows_read, a2.rows_written), (a1.requests, a1.steps, a1.rows_read, a1.rows_written));
    // a stopped tenant's usage is kept for the next call
    http(port, "a.test", "POST", "/v1/query", Some(json!({ "entity": "Note", "fields": ["text"] })), Some(&session));
    assert!(rt.block_on(host.stop("ta")));
    // counting users never starts a tenant
    assert_eq!(rt.block_on(host.user_count("a.test")).unwrap(), None);
    assert!(host.running().iter().all(|t| t.id != "ta"), "still stopped");
    assert_eq!(host.take_usage().into_iter().find(|(t, _)| t == "ta").map(|(_, u)| u.requests), Some(1));
    std::fs::remove_dir_all(&base).ok();
}
