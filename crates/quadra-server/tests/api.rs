//! The API over real HTTP: authentication, queries with user expressions, idempotent
//! transactions, refused stale writes, CSRF on browser sessions.

use quadra_sema::Project;
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

struct Http {
    port: u16,
}

impl Http {
    fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&J>) -> (u16, J, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n", self.port, body.len());
        if !body.is_empty() {
            req.push_str("Content-Type: application/json\r\n");
        }
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(&body);
        s.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
            let mut out = String::new();
            let mut r = rest;
            while let Some((size, tail)) = r.split_once("\r\n") {
                let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
                if n == 0 {
                    break;
                }
                out.push_str(&tail[..n]);
                r = &tail[n + 2..];
            }
            out
        } else {
            rest.to_string()
        };
        (status, serde_json::from_str(&body).unwrap_or(J::Null), head.to_string())
    }
}

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

#[test]
fn the_api_is_authenticated_idempotent_and_refuses_stale_writes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let file = std::env::temp_dir().join(format!("quadra-api-{}.db", std::process::id()));
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", file.display()));
    }
    let url = format!("sqlite:{}", file.display());
    let project = Project::load(&root).unwrap();
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, now()).unwrap();
    ops::fixture(&url, m, r, "italy", now()).unwrap();
    // a token for giulia (Salesperson, Buyer)
    let mut db = quadra_store::Database::open(&url, m, r).unwrap();
    let mut interp = ops::loaded(&mut db, m, now()).unwrap();
    let users = interp.ids("User", None);
    let giulia = users.into_iter().find(|id| interp.store.read(*id, "login", None).map_or(false, |v| v.to_string() == "giulia")).unwrap();
    let mut conn = Conn::open(&url).unwrap();
    let (token, _) = auth::create(&mut conn, "token", giulia, "test", false, now(), None).unwrap();

    let port = 18000 + (std::process::id() % 1000) as u16;
    let cfg = quadra_server::Config { root: root.clone(), url: url.clone(), port, dev_login: true, secure_cookies: false, cache_mb: 16, prewarm_days: None, web: None, sweep_every: 0, editors: vec![], watch: false, limits: Default::default(), engines: Default::default() };
    std::thread::spawn(move || quadra_server::serve(cfg).unwrap());
    let http = Http { port };
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let bearer = format!("Bearer {token}");
    let auth = [("Authorization", bearer.as_str())];

    // authentication
    assert_eq!(http.call("GET", "/v1/me", &[], None).0, 401);
    assert_eq!(http.call("GET", "/v1/me", &[("Authorization", "Bearer qd_t_forged")], None).0, 401);
    let (s, me, head) = http.call("GET", "/v1/me", &auth, None);
    assert_eq!((s, me["user"].clone()), (200, json!("giulia")));
    assert!(head.to_lowercase().contains("x-frame-options: deny"), "{head}");
    // the custom forms of apps are reached over HTTP (an unknown app: 404, not 405)
    let (s, _, _) = http.call("POST", "/v1/apps/999999/forms/f/submit", &auth, Some(&json!({ "values": {}, "idempotency_key": "k" })));
    assert_eq!(s, 404);

    // a query with a filter and a personal column; hidden fields stay hidden
    let (s, q, _) = http.call("POST", "/v1/query", &auth, Some(&json!({ "entity": "Product", "fields": ["code", "standard_cost"], "where": "type is Storable", "columns": { "double": "list_price * 2" } })));
    assert_eq!(s, 200, "{q}");
    let row = &q["rows"][0];
    assert_eq!(row["cells"]["code"]["d"], json!("W-01"));
    assert_eq!(row["cells"]["standard_cost"]["hidden"], json!(true));
    assert!(row["cells"]["double"]["d"].as_str().unwrap().starts_with("20"), "{row}");
    // an expression that does not type check, and one that tries to add code
    let (s, e, _) = http.call("POST", "/v1/query", &auth, Some(&json!({ "entity": "Product", "columns": { "x": "list_price + 3 kg" } })));
    assert_eq!(s, 400);
    assert!(e["diagnostics"][0].as_str().unwrap().contains("cannot add"), "{e}");
    let (s, _, _) = http.call("POST", "/v1/query", &auth, Some(&json!({ "entity": "Product", "columns": { "x": "1\n    action evil:\n        fail \"x\"" } })));
    assert_eq!(s, 400);
    let base = q["base"].as_u64().unwrap();

    // create a sales order: idempotent
    let rossi = http.call("POST", "/v1/query", &auth, Some(&json!({ "entity": "Partner", "where": "name == \"Rossi Spa\"", "fields": [] }))).1["rows"][0]["id"].as_u64().unwrap();
    let create = json!({ "ops": [{ "op": "create", "entity": "SalesOrder", "ref": "so", "values": { "customer": rossi, "lines": [{ "product": row["id"], "qty": "3 pcs" }] } }] });
    assert_eq!(http.call("POST", "/v1/tx", &auth, Some(&create)).0, 400, "Idempotency-Key is required");
    let key = [("Authorization", bearer.as_str()), ("Idempotency-Key", "order-1")];
    let (s, first, _) = http.call("POST", "/v1/tx", &key, Some(&create));
    assert_eq!(s, 200, "{first}");
    let (s, again, _) = http.call("POST", "/v1/tx", &key, Some(&create));
    assert_eq!((s, again["replayed"].clone(), again["created"].clone()), (200, json!(true), first["created"].clone()));
    let other = json!({ "ops": [{ "op": "create", "entity": "SalesOrder", "values": { "customer": rossi } }] });
    assert_eq!(http.call("POST", "/v1/tx", &key, Some(&other)).0, 422, "same key, different request");
    let so = first["created"]["so"].as_u64().unwrap();
    let (_, rec, _) = http.call("GET", &format!("/v1/records/SalesOrder/{so}"), &auth, None);
    assert_eq!(rec["cells"]["total"]["d"], json!("36.60 EUR"));
    // the field of a flow is moved by its actions, never written
    let state = json!({ "base": rec["base"], "ops": [{ "op": "update", "entity": "SalesOrder", "id": so, "values": { "state": "Confirmed" } }] });
    let (s, e, _) = http.call("POST", "/v1/tx", &[("Authorization", bearer.as_str()), ("Idempotency-Key", "state")], Some(&state));
    assert_eq!(s, 422, "{e}");
    assert!(e.to_string().contains("`SalesOrder.state` changes only through its flow: run one of its actions (send, confirm, lock, cancel)"), "{e}");

    // two edits from the same base: the second is refused with who changed what
    let line = rec["cells"]["lines"]["v"]["list"][0]["ref"].as_u64().unwrap();
    let base2 = rec["base"].as_u64().unwrap();
    assert!(base2 > base);
    let edit = |qty: &str| json!({ "base": base2, "ops": [{ "op": "update", "entity": "SalesLine", "id": line, "values": { "qty": qty } }] });
    let (s, _, _) = http.call("POST", "/v1/tx", &[("Authorization", bearer.as_str()), ("Idempotency-Key", "edit-a")], Some(&edit("5 pcs")));
    assert_eq!(s, 200);
    let (s, c, _) = http.call("POST", "/v1/tx", &[("Authorization", bearer.as_str()), ("Idempotency-Key", "edit-b")], Some(&edit("7 pcs")));
    assert_eq!(s, 409, "{c}");
    assert_eq!(c["conflicts"][0]["what"][0], json!(format!("SalesLine#{line}.qty")));
    assert_eq!(c["conflicts"][0]["user"], json!("giulia"));
    // values from outside are checked against the declared unit
    let latest = http.call("GET", &format!("/v1/records/SalesOrder/{so}"), &auth, None).1["base"].as_u64().unwrap();
    let kg = json!({ "base": latest, "ops": [{ "op": "update", "entity": "SalesLine", "id": line, "values": { "qty": "2 kg" } }] });
    let (s, e, _) = http.call("POST", "/v1/tx", &[("Authorization", bearer.as_str()), ("Idempotency-Key", "edit-kg")], Some(&kg));
    assert_eq!(s, 422, "{e}");
    // permissions
    let vat = json!({ "base": latest, "ops": [{ "op": "update", "entity": "Partner", "id": rossi, "values": { "vat_number": "IT00000000000" } }] });
    assert_eq!(http.call("POST", "/v1/tx", &[("Authorization", bearer.as_str()), ("Idempotency-Key", "vat")], Some(&vat)).0, 403);

    // a browser session: writes need the CSRF token and the same origin
    let (s, login, head) = http.call("POST", "/v1/auth/dev-login", &[], Some(&json!({ "login": "giulia" })));
    assert_eq!(s, 200);
    let cookie = head.lines().find(|l| l.to_lowercase().starts_with("set-cookie:")).unwrap().split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string();
    assert!(head.to_lowercase().contains("httponly") && head.to_lowercase().contains("samesite=strict"));
    let csrf = login["csrf"].as_str().unwrap().to_string();
    let q = json!({ "entity": "Currency", "limit": 1 });
    assert_eq!(http.call("POST", "/v1/query", &[("Cookie", cookie.as_str())], Some(&q)).0, 403);
    assert_eq!(http.call("POST", "/v1/query", &[("Cookie", cookie.as_str()), ("X-CSRF-Token", csrf.as_str()), ("Origin", "https://evil.example")], Some(&q)).0, 403);
    assert_eq!(http.call("POST", "/v1/query", &[("Cookie", cookie.as_str()), ("X-CSRF-Token", csrf.as_str())], Some(&q)).0, 200);
    assert_eq!(http.call("GET", "/v1/me", &[("Cookie", cookie.as_str())], None).0, 200);
}

/// The code editor: only editors; files inside the project only; a commit with errors is
/// refused; a good one is committed in git, applied to the database and served.
#[test]
fn the_code_editor_commits_and_deploys() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let root = std::env::temp_dir().join(format!("quadra-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd '{}' && tar --exclude .quadra -cf - . | (cd '{}' && tar xf -) && cd '{}' && git init -q -b main && git add -A && git -c user.name=t -c user.email=t@t commit -q -m init",
            src.display(),
            root.display(),
            root.display()
        ))
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not prepare a git copy of the project");
    let file = root.join(".quadra/data.db");
    std::fs::create_dir_all(root.join(".quadra")).unwrap();
    let url = format!("sqlite:{}", file.display());
    let project = Project::load(&root).unwrap();
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, now()).unwrap();
    ops::fixture(&url, m, r, "italy", now()).unwrap();
    std::fs::write(root.join(".gitignore"), ".quadra/\n").unwrap();

    let port = 19000 + (std::process::id() % 1000) as u16;
    let cfg = quadra_server::Config {
        root: root.clone(),
        url: url.clone(),
        port,
        dev_login: true,
        secure_cookies: false,
        cache_mb: 16,
        prewarm_days: None,
        web: None,
        sweep_every: 0,
        editors: vec!["admin".into()],
        watch: false,
        limits: Default::default(),
        engines: Default::default(),
    };
    std::thread::spawn(move || quadra_server::serve(cfg).unwrap());
    let http = Http { port };
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let session = |login: &str| -> (String, String) {
        let (_, body, head) = http.call("POST", "/v1/auth/dev-login", &[], Some(&json!({ "login": login })));
        let cookie = head.lines().find(|l| l.to_lowercase().starts_with("set-cookie:")).unwrap().split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string();
        (cookie, body["csrf"].as_str().unwrap().to_string())
    };
    // only editors
    let (giulia, gcsrf) = session("giulia");
    assert_eq!(http.call("GET", "/v1/dev/files", &[("Cookie", giulia.as_str())], None).0, 403);
    assert_eq!(http.call("POST", "/v1/dev/check", &[("Cookie", giulia.as_str()), ("X-CSRF-Token", gcsrf.as_str())], Some(&json!({ "buffers": {} }))).0, 403);
    let (admin, csrf) = session("admin");
    let h = [("Cookie", admin.as_str()), ("X-CSRF-Token", csrf.as_str())];
    let (s, files, _) = http.call("GET", "/v1/dev/files", &h, None);
    assert_eq!(s, 200);
    assert!(files["files"].as_array().unwrap().iter().any(|f| f["path"] == json!("sales.qd")));
    // files inside the project only
    assert_eq!(http.call("PUT", "/v1/dev/file", &h, Some(&json!({ "path": "../escape.qd", "text": "x" }))).0, 400);
    assert_eq!(http.call("GET", "/v1/dev/file?path=.git/config", &h, None).0, 400);
    // editor features: definition in another file, hover with the type, completion after a
    // dot while the code is incomplete, values on the live data
    let src_sales = http.call("GET", "/v1/dev/file?path=sales.qd", &h, None).1["text"].as_str().unwrap().to_string();
    let at = |needle: &str, k: usize| src_sales[..src_sales.find(needle).unwrap() + k].encode_utf16().count();
    let ide = |op: &str, offset: usize, buffers: J| http.call("POST", "/v1/dev/ide", &h, Some(&json!({ "op": op, "path": "sales.qd", "offset": offset, "buffers": buffers }))).1;
    let def = ide("definition", at("customer: Partner", 11), json!({}));
    assert_eq!(def["path"], json!("partners.qd"), "{def}");
    let hover = ide("hover", at("customer: Partner", 2), json!({}));
    assert!(hover["markdown"].as_str().unwrap().contains("Type: `Partner`"), "{hover}");
    let typing = src_sales.replacen("    invoice_address: Address default customer.address", "    invoice_address: Address default customer.", 1);
    let pos = typing[..typing.find("default customer.").unwrap() + "default customer.".len()].encode_utf16().count();
    let c = http.call("POST", "/v1/dev/ide", &h, Some(&json!({ "op": "complete", "path": "sales.qd", "offset": pos, "buffers": { "sales.qd": typing } }))).1;
    let labels: Vec<&str> = c["options"].as_array().unwrap().iter().filter_map(|o| o["label"].as_str()).collect();
    assert!(labels.contains(&"address") && labels.contains(&"as_customer") && !labels.contains(&"lines"), "{labels:?}");
    let field = ide("inspect", at("    state:", 6), json!({}));
    assert_eq!(field["kind"], json!("field"), "{field}");
    // on the operator of a check (on a field name the field wins)
    let check = ide("inspect", at("check valid_until >=", 19), json!({}));
    assert_eq!(check["kind"], json!("check"), "{check}");
    assert_eq!(check["label"], json!("valid_until >= date"), "{check}");

    // files and folders: create, rename, delete; never outside the project
    assert_eq!(http.call("POST", "/v1/dev/dir", &h, Some(&json!({ "path": "reports" }))).0, 200);
    assert_eq!(http.call("POST", "/v1/dev/dir", &h, Some(&json!({ "path": "../out" }))).0, 400);
    assert_eq!(http.call("POST", "/v1/dev/dir", &h, Some(&json!({ "path": ".git/hooks" }))).0, 400);
    let (_, files, _) = http.call("GET", "/v1/dev/files", &h, None);
    assert!(files["dirs"].as_array().unwrap().contains(&json!("reports")), "{files}");
    let kpi = "entity Kpi:\n    customer: Partn";
    assert_eq!(http.call("PUT", "/v1/dev/file", &h, Some(&json!({ "path": "reports/kpi.qd", "text": kpi }))).0, 200);
    // auto import: completing a name of another module adds its import
    let c = http.call("POST", "/v1/dev/ide", &h, Some(&json!({ "op": "complete", "path": "reports/kpi.qd", "offset": kpi.len(), "buffers": {} }))).1;
    let partner = c["options"].as_array().unwrap().iter().find(|o| o["label"] == json!("Partner")).cloned().unwrap_or_else(|| panic!("{c}"));
    assert_eq!(partner["edit"]["insert"], json!("from partners import Partner\n\n"), "{partner}");
    assert_eq!(partner["edit"]["from"], json!(0), "{partner}");
    // ... and the error of a name not imported has the same fix
    let typed = "entity Kpi:\n    customer: Partner\n";
    let (_, d, _) = http.call("POST", "/v1/dev/check", &h, Some(&json!({ "buffers": { "reports/kpi.qd": typed } })));
    let fix = d["diagnostics"].as_array().unwrap().iter().find_map(|d| d["fix"].as_object().cloned()).unwrap_or_else(|| panic!("{d}"));
    assert_eq!(fix["insert"], json!("from partners import Partner\n\n"), "{d}");
    let fixed = format!("{}{}", fix["insert"].as_str().unwrap(), typed);
    let (_, d, _) = http.call("POST", "/v1/dev/check", &h, Some(&json!({ "buffers": { "reports/kpi.qd": fixed } })));
    assert!(d["diagnostics"].as_array().unwrap().iter().all(|d| d["path"] != json!("reports/kpi.qd")), "{d}");
    // rename: a file, never onto another one or out of the project
    assert_eq!(http.call("POST", "/v1/dev/rename", &h, Some(&json!({ "from": "reports/kpi.qd", "to": "reports/dash.qd" }))).0, 200);
    assert_eq!(http.call("POST", "/v1/dev/rename", &h, Some(&json!({ "from": "reports/dash.qd", "to": "sales.qd" }))).0, 409);
    assert_eq!(http.call("POST", "/v1/dev/rename", &h, Some(&json!({ "from": "reports/dash.qd", "to": "../dash.qd" }))).0, 400);
    assert_eq!(http.call("POST", "/v1/dev/rename", &h, Some(&json!({ "from": "reports", "to": ".git" }))).0, 400);
    assert!(root.join("reports/dash.qd").exists());
    // a folder is deleted only if everything in it could be deleted from here
    assert_eq!(http.call("DELETE", "/v1/dev/dir?path=", &h, None).0, 400);
    assert_eq!(http.call("DELETE", "/v1/dev/dir?path=.git", &h, None).0, 400);
    std::fs::write(root.join("reports/.secret"), "x").unwrap();
    assert_eq!(http.call("DELETE", "/v1/dev/dir?path=reports", &h, None).0, 409);
    std::fs::remove_file(root.join("reports/.secret")).unwrap();
    let (s, del, _) = http.call("DELETE", "/v1/dev/dir?path=reports", &h, None);
    assert_eq!(s, 200, "{del}");
    assert_eq!(del["deleted"], json!(["reports/dash.qd"]));
    assert!(!root.join("reports").exists());

    // the data model of the code being edited, against the model running
    let schema = |buffers: J| http.call("POST", "/v1/dev/schema", &h, Some(&json!({ "buffers": buffers }))).1;
    let same = schema(json!({}));
    assert_eq!(same["stale"], json!(false));
    assert_eq!(same["diff"], json!([]), "{}", same["diff"]);
    let so = same["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesOrder")).cloned().unwrap();
    let lines = so["fields"].as_array().unwrap().iter().find(|f| f["name"] == json!("lines")).cloned().unwrap();
    assert_eq!((lines["target"].clone(), lines["many"].clone()), (json!("SalesLine"), json!(true)), "{lines}");
    assert_eq!(so["place"]["path"], json!("sales.qd"));
    let sl = same["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesLine")).cloned().unwrap();
    assert!(sl["fields"].as_array().unwrap().iter().any(|f| f["parent"] == json!(true) && f["target"] == json!("SalesOrder")), "{sl}");
    // a field renamed without a migration: removed, and the new one is suspected
    let renamed = src_sales.replace("valid_until", "expires").replace("    customer: Partner", "    channel: Text?\n    customer: Partner");
    let d = schema(json!({ "sales.qd": renamed }));
    let so = d["diff"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesOrder")).cloned().unwrap_or_else(|| panic!("{d}"));
    let change = |f: &str| so["fields"].as_array().unwrap().iter().find(|c| c["field"] == json!(f)).cloned().unwrap_or_else(|| panic!("{so}"));
    assert_eq!(change("valid_until")["change"], json!("removed"));
    assert_eq!(change("valid_until")["maybe_renamed_to"], json!(["expires"]));
    assert_eq!(change("expires")["change"], json!("added"));
    assert_eq!(change("channel")["change"], json!("added"));
    // with its migration: a rename
    let migration = "migration expiry:\n    rename SalesOrder.valid_until -> SalesOrder.expires\n";
    let d = schema(json!({ "sales.qd": renamed, "migrations/2027-02-01_expiry.qd": migration }));
    let so = d["diff"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesOrder")).cloned().unwrap();
    let expires = so["fields"].as_array().unwrap().iter().find(|c| c["field"] == json!("expires")).cloned().unwrap_or_else(|| panic!("{so}"));
    assert_eq!((expires["change"].clone(), expires["from"].clone(), expires["migration"].clone()), (json!("renamed"), json!("valid_until"), json!("expiry")), "{so}");
    assert!(so["fields"].as_array().unwrap().iter().all(|c| c["field"] != json!("valid_until")), "{so}");
    // code that does not compile: the last version that did, said so
    let broken = schema(json!({ "sales.qd": "entity SalesOrder is Document:\n    customer: Nope\n" }));
    assert_eq!(broken["stale"], json!(true), "{broken}");
    assert!(!broken["entities"].as_array().unwrap().is_empty());

    // hints: how the queries, rules and formulas of the code meet the database
    let (_, d, _) = http.call("POST", "/v1/dev/check", &h, Some(&json!({ "buffers": {} })));
    let hints: Vec<&J> = d["diagnostics"].as_array().unwrap().iter().filter(|x| x["severity"] == json!("hint")).collect();
    assert!(hints.iter().any(|x| x["message"].as_str().unwrap().contains("runs in SQL")), "{d}");
    assert!(d["diagnostics"].as_array().unwrap().iter().all(|x| x["severity"] != json!("error")), "{d}");
    // an error, seen by the check and refusing the commit
    let sales = http.call("GET", "/v1/dev/file?path=sales.qd", &h, None).1["text"].as_str().unwrap().to_string();
    let wrong = sales.replace("    invoice_address:", "    oops: Money<EUR> default 3 kg\n    invoice_address:");
    let (_, d, _) = http.call("POST", "/v1/dev/check", &h, Some(&json!({ "buffers": { "sales.qd": wrong } })));
    assert!(d["diagnostics"][0]["message"].as_str().unwrap().contains("cannot store Qty<kg>"), "{d}");
    assert_eq!(http.call("PUT", "/v1/dev/file", &h, Some(&json!({ "path": "sales.qd", "text": wrong }))).0, 200);
    let (s, c, _) = http.call("POST", "/v1/dev/commit", &h, Some(&json!({ "message": "broken" })));
    assert_eq!(s, 422, "{c}");
    assert_eq!(c["steps"][0]["step"], json!("check"));
    // a good change: committed, applied, served
    let good = sales.replace("    invoice_address:", "    customer_ref: Text?\n    invoice_address:");
    http.call("PUT", "/v1/dev/file", &h, Some(&json!({ "path": "sales.qd", "text": good })));
    let (s, c, _) = http.call("POST", "/v1/dev/commit", &h, Some(&json!({ "message": "Customer reference" })));
    assert_eq!(s, 200, "{c}");
    let sha = c["sha"].as_str().unwrap().to_string();
    let (_, st, _) = http.call("GET", "/v1/dev/status", &h, None);
    assert_eq!(st["deployed"], json!(sha));
    assert_eq!(st["commits"][0]["author"], json!("admin"));
    assert!(st["changes"].as_object().unwrap().is_empty(), "{st}");
    let (_, model, _) = http.call("GET", "/v1/model", &h, None);
    let so = model["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesOrder")).unwrap().clone();
    assert!(so["fields"].as_array().unwrap().iter().any(|f| f["name"] == json!("customer_ref")), "the server serves the new model");
    assert_eq!(ops::verify(&url, m, r, now()).map(|v| v.0.len()).unwrap_or(1), 0);
}

#[test]
fn the_dev_server_applies_saved_files() {
    // the project `quadra init` writes
    let tpl = Path::new(env!("CARGO_MANIFEST_DIR")).join("../quadra-cli/templates/starter");
    let root = std::env::temp_dir().join(format!("quadra-dev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("quadra.toml"), "[project]\nname = \"dev-test\"\n").unwrap();
    std::fs::copy(tpl.join("main.qd"), root.join("main.qd")).unwrap();
    std::fs::copy(tpl.join("tests/main.test.qd"), root.join("tests/main.test.qd")).unwrap();
    let url = format!("sqlite:{}", root.join("data.db").display());
    let project = Project::load(&root).unwrap();
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, now()).unwrap();
    ops::fixture(&url, m, r, "demo", now()).unwrap();
    assert!(ops::ensure_user(&url, m, r, "dev", now()).unwrap());
    assert!(!ops::ensure_user(&url, m, r, "dev", now()).unwrap(), "only when there is none");

    let port = 20000 + (std::process::id() % 1000) as u16;
    let cfg =
        quadra_server::Config { root: root.clone(), url: url.clone(), port, dev_login: true, secure_cookies: false, cache_mb: 16, prewarm_days: None, web: None, sweep_every: 0, editors: vec!["*".into()], watch: true, limits: Default::default(), engines: Default::default() };
    std::thread::spawn(move || quadra_server::serve(cfg).unwrap());
    let http = Http { port };
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // signed in without a login: the first user
    let (s, body, head) = http.call("POST", "/v1/auth/dev-login", &[], Some(&json!({ "login": "" })));
    assert_eq!((s, body["user"].clone()), (200, json!("dev")), "{body}");
    let cookie = head.lines().find(|l| l.to_lowercase().starts_with("set-cookie:")).unwrap().split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string();
    let h = [("Cookie", cookie.as_str())];
    // with no roles in the model, everything is allowed
    assert!(http.call("GET", "/v1/dev/files", &h, None).0 == 200, "editors: everyone");
    let fields = |http: &Http| -> Vec<String> {
        let (_, model, _) = http.call("GET", "/v1/model", &h, None);
        model["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("Customer")).unwrap()["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap().to_string()).collect()
    };
    let wait = |http: &Http, until: &dyn Fn(&J, &[String]) -> bool| -> J {
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(200));
            let live = http.call("GET", "/v1/dev/live", &h, None).1;
            if until(&live, &fields(http)) {
                return live;
            }
        }
        panic!("the dev server did not apply the change: {}", http.call("GET", "/v1/dev/live", &h, None).1);
    };
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    // a new field: applied
    std::fs::write(root.join("main.qd"), main.replace("    email: Text?\n", "    email: Text?\n    phone: Text?\n")).unwrap();
    wait(&http, &|live, f| live["problems"].is_null() && f.contains(&"phone".to_string()));
    // code that does not compile: reported, the last version keeps running
    std::fs::write(root.join("main.qd"), main.replace("    email: Text?\n", "    email: Text?\n    phone: Text? default 5 EUR\n")).unwrap();
    let live = wait(&http, &|live, _| live["problems"]["kind"] == json!("code"));
    assert!(live["problems"]["errors"][0]["message"].as_str().unwrap().contains("cannot store Money<EUR>"), "{live}");
    assert!(fields(&http).contains(&"phone".to_string()));
    // a required field the data lacks: refused with the reason
    std::fs::write(root.join("main.qd"), main.replace("    email: Text?\n", "    email: Text?\n    vat: Text\n")).unwrap();
    let live = wait(&http, &|live, _| live["problems"]["kind"] == json!("data"));
    assert!(live["problems"]["message"].as_str().unwrap().contains("Customer.vat is required"), "{live}");
    // with a default it fits; a rename with its migration keeps the values
    std::fs::write(root.join("main.qd"), main.replace("    email: Text?\n", "    mail: Text?\n    vat: Text default \"n/a\"\n")).unwrap();
    std::fs::write(root.join("tests/main.test.qd"), std::fs::read_to_string(root.join("tests/main.test.qd")).unwrap().replace("email=", "mail=")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(root.join("migrations/2026-11-22_mail.qd"), "migration mail:\n    rename Customer.email -> Customer.mail\n").unwrap();
    wait(&http, &|live, f| live["problems"].is_null() && f.contains(&"mail".to_string()) && f.contains(&"vat".to_string()));
    let csrf = body["csrf"].as_str().unwrap().to_string();
    let (_, rows, _) = http.call("POST", "/v1/query", &[("Cookie", cookie.as_str()), ("X-CSRF-Token", csrf.as_str())], Some(&json!({ "entity": "Customer" })));
    let acme = &rows["rows"][0]["cells"];
    assert_eq!((acme["mail"]["d"].clone(), acme["vat"]["d"].clone()), (json!("buyer@acme.test"), json!("n/a")), "{rows}");
    // a rule the data breaks: the rule's line, the records and the values that break it
    let now_main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    std::fs::write(root.join("main.qd"), now_main.replace("    vat: Text default \"n/a\"\n", "    vat: Text default \"n/a\"\n    check vat != \"n/a\", \"Give the VAT number\"\n")).unwrap();
    let live = wait(&http, &|live, _| live["problems"]["kind"] == json!("data"));
    let msg = live["problems"]["message"].as_str().unwrap();
    for part in ["main.qd:", "check vat != \"n/a\", \"Give the VAT number\"", "breaks it:", "vat = n/a", "add a migration"] {
        assert!(msg.contains(part), "missing {part:?} in {msg}");
    }
    // correcting the data in the app applies the saved code, without saving it again
    let id = rows["rows"][0]["id"].as_u64().unwrap();
    let base = rows["base"].as_u64().unwrap();
    let auth = [("Cookie", cookie.as_str()), ("X-CSRF-Token", csrf.as_str()), ("Idempotency-Key", "vat-fix")];
    let (s, out, _) = http.call("POST", "/v1/tx", &auth, Some(&json!({ "base": base, "ops": [{ "op": "update", "entity": "Customer", "id": id, "values": { "vat": "IT01234567890" } }] })));
    assert_eq!(s, 200, "{out}");
    wait(&http, &|live, _| live["problems"].is_null());
}
