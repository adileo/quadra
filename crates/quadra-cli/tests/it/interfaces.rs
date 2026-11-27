//! Apps and views from interface files (`interfaces/**/*.qdi`, engine/interfaces.rs) over the
//! engine of the server (`quadra_server::Local`): installed when the server starts (folders
//! from directories), read-only in production ("defined in …", shares with users added on
//! top), exported as `.qdi`, duplicated as data; written back in `quadra dev` (a change, a
//! move, a delete), installed again when a file changes; a file that does not check is not
//! served; and the custom form of `examples/stress/interfaces/sales/desk.qdi` submitted as
//! the viewer (idempotent, `where` at the field, permissions). On SQLite and, when
//! `QUADRA_TEST_PG` holds a url, on PostgreSQL.

use quadra_server::Local;
use quadra_store::conn::Conn;
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("qd" | "toml" | "qdi")
        ) {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

fn project_copy(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir = std::env::temp_dir().join(format!("quadra-qdi-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml")) {
            std::fs::copy(&p, dir.join(e.file_name())).unwrap();
        }
    }
    copy_dir(&src.join("interfaces"), &dir.join("interfaces"));
    dir
}

fn fresh(target: &str, tag: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-qdi-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qi_{}_{tag}", std::process::id());
        Conn::open(target)
            .unwrap()
            .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
            .unwrap();
        format!("{target}#schema={schema}")
    }
}

fn cleanup(target: &str, url: &str, root: &Path) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{s}"));
        }
    } else if let Some((_, schema)) = url.split_once("#schema=") {
        let _ = Conn::open(target)
            .and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    }
    let _ = std::fs::remove_dir_all(root);
}

fn world(target: &str, tag: &str) -> (PathBuf, String) {
    let root = project_copy(tag);
    let url = fresh(target, tag);
    let dir = std::env::temp_dir().join(format!("quadra-qdi-{}-{tag}-gen", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
        .args(["bench", "gen", "--scale", "tiny", "--out"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    let checked = quadra_analysis::check(&quadra_sema::Project::load(&root).unwrap());
    assert!(
        checked
            .diagnostics
            .iter()
            .all(|d| d.severity != quadra_sema::Severity::Error),
        "{:?}",
        checked.diagnostics
    );
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    quadra_store::ops::init(&url, m, r, clock()).unwrap();
    quadra_store::bulk::import_dir(&url, m, r, &dir, clock(), None, &mut |_| {}).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (root, url)
}

fn pick(url: &str, sql: &str) -> u64 {
    Conn::open(url)
        .unwrap()
        .query(sql, &[])
        .unwrap()
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no row: {sql}"))
}

fn call(l: &mut Local, user: u64, method: &str, path: &str, body: J) -> (u16, J) {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let q: Vec<(&str, &str)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|p| p.split_once('='))
        .collect();
    l.views(user, method, path, &q, &body)
}

fn ok(l: &mut Local, user: u64, method: &str, path: &str, body: J) -> J {
    let (s, b) = call(l, user, method, path, body);
    assert!(s == 200 || s == 201, "{method} {path} as #{user}: {s} {b}");
    b
}

fn app_named(l: &mut Local, user: u64, name: &str) -> J {
    let b = ok(l, user, "GET", "/v1/views?kind=app", J::Null);
    b["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| panic!("no app {name}: {b}"))
}

fn interfaces_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| {
        pick(
            &url,
            &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"),
        )
    };
    let (admin, s1, s2) = (user("admin"), user("sales0001"), user("sales0002"));
    let logistics = pick(&url, "SELECT CAST(u.\"id\" AS TEXT) FROM \"user\" u WHERE u.\"login\" LIKE 'log%' ORDER BY u.\"id\" LIMIT 1");

    // ---- installed when the server starts, read-only in production ----
    let mut l = Local::open(&root, &url, 64).unwrap();
    let cockpit = app_named(&mut l, admin, "Sales cockpit");
    assert_eq!(
        (
            cockpit["origin"].clone(),
            cockpit["file"].clone(),
            cockpit["read_only"].clone(),
            cockpit["can_edit"].clone()
        ),
        (
            json!("file"),
            json!("interfaces/sales/cockpit.qdi"),
            json!(true),
            json!(false)
        ),
        "{cockpit}"
    );
    let id = cockpit["id"].as_u64().unwrap();
    let folders = ok(&mut l, admin, "GET", "/v1/views?kind=app", J::Null)["folders"].clone();
    let sales = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("sales"))
        .expect("the folder of the directory");
    assert_eq!(cockpit["parent"], sales["id"]);
    // the file's shares: everyone uses it (a salesperson sees it)
    app_named(&mut l, s1, "Sales cockpit");
    // the views of a file
    let views = ok(
        &mut l,
        admin,
        "GET",
        "/v1/views?kind=view&entity=Order&scope=all",
        J::Null,
    )["views"]
        .clone();
    assert!(
        views
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == json!("To ship")
                && v["file"] == json!("interfaces/sales/orders.qdi")),
        "{views}"
    );
    // not changed through the API
    let v = cockpit["version"].as_u64().unwrap();
    let (s, b) = call(
        &mut l,
        admin,
        "PUT",
        &format!("/v1/views/{id}"),
        json!({ "base": v, "name": "Mine" }),
    );
    assert_eq!(s, 409, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .contains("defined in interfaces/sales/cockpit.qdi"),
        "{b}"
    );
    assert_eq!(
        call(
            &mut l,
            admin,
            "DELETE",
            &format!("/v1/views/{id}?base={v}"),
            J::Null
        )
        .0,
        409
    );
    // shared with a user on top of the file's shares; the file's own are not changed here
    let file_share = json!({ "type": "everyone", "edit": false });
    ok(
        &mut l,
        admin,
        "PUT",
        &format!("/v1/views/{id}/shares"),
        json!({ "base": v, "shares": [file_share, { "type": "user", "id": s2, "edit": true }] }),
    );
    let v2 = ok(&mut l, admin, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["version"]
        .as_u64()
        .unwrap();
    let (s, _) = call(
        &mut l,
        admin,
        "PUT",
        &format!("/v1/views/{id}/shares"),
        json!({ "base": v2, "shares": [{ "type": "role", "name": "Admin", "edit": true }] }),
    );
    assert_eq!(s, 409);
    // exported as the file says
    let exported = ok(
        &mut l,
        admin,
        "GET",
        &format!("/v1/views/{id}/qdi"),
        J::Null,
    );
    let on_disk = std::fs::read_to_string(root.join("interfaces/sales/cockpit.qdi")).unwrap();
    assert_eq!(exported["text"].as_str().unwrap(), on_disk);
    // duplicated: an app of one's own, made of data
    let theirs = app_named(&mut l, s1, "Sales cockpit")["config"].clone();
    let copy = ok(
        &mut l,
        s1,
        "POST",
        "/v1/views",
        json!({ "kind": "app", "name": "My cockpit", "config": theirs }),
    )["view"]
        .clone();
    assert_eq!(
        (copy["origin"].clone(), copy["can_edit"].clone()),
        (J::Null, json!(true))
    );

    // ---- the custom form: as the viewer, in one transaction ----
    let desk = app_named(&mut l, s1, "Order desk");
    let desk_id = desk["id"].as_u64().unwrap();
    let customer = pick(&url, &format!("SELECT CAST(c.\"id\" AS TEXT) FROM \"customer\" c WHERE c.\"salesperson_id\" = {s1} AND c.\"active\" ORDER BY c.\"id\" LIMIT 1"));
    let product = pick(&url, "SELECT CAST(p.\"id\" AS TEXT) FROM \"product\" p WHERE p.\"active\" ORDER BY p.\"id\" LIMIT 1");
    let submit = |l: &mut Local, who: u64, values: J, key: &str| {
        call(
            l,
            who,
            "POST",
            &format!("/v1/apps/{desk_id}/forms/quick/submit"),
            json!({ "values": values, "params": [], "idempotency_key": key }),
        )
    };
    let values = json!({ "number": "QD-0001", "customer": customer, "product": product, "qty": 3 });
    let (s, b) = submit(&mut l, s1, values.clone(), "k1");
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["result"]["entity"], json!("Order"), "{b}");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .starts_with("Order QD-0001: 3 pcs of "),
        "{b}"
    );
    let order = b["result"]["id"].as_u64().unwrap();
    assert_eq!(pick(&url, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"order_line\" l WHERE l.\"order_id\" = {order}")), 1);
    // the same key, the same values: the same answer, nothing written twice
    let (s, b2) = submit(&mut l, s1, values.clone(), "k1");
    assert_eq!(
        (s, b2["replayed"].clone(), b2["result"]["id"].clone()),
        (200, json!(true), json!(order))
    );
    // the same key for other values
    assert_eq!(
        submit(
            &mut l,
            s1,
            json!({ "number": "QD-0002", "customer": customer, "product": product, "qty": 3 }),
            "k1"
        )
        .0,
        409
    );
    // a `where` of a field, a missing field: at the field
    let (s, b) = submit(
        &mut l,
        s1,
        json!({ "number": "QD-0003", "customer": customer, "product": product, "qty": 0 }),
        "k2",
    );
    assert_eq!((s, b["field"].clone()), (422, json!("qty")), "{b}");
    let (s, b) = submit(
        &mut l,
        s1,
        json!({ "customer": customer, "product": product, "qty": 1 }),
        "k3",
    );
    assert_eq!((s, b["field"].clone()), (422, json!("number")), "{b}");
    // what the viewer may not do: refused, nothing written
    let before = pick(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"order\"");
    let (s, b) = submit(
        &mut l,
        logistics,
        json!({ "number": "QD-0004", "customer": customer, "product": product, "qty": 1 }),
        "k4",
    );
    assert!(s == 403 || s == 404 || s == 422, "{s} {b}");
    assert_eq!(
        pick(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"order\""),
        before
    );
    // a `require` of the code (`confirm` of an order without lines is refused by its action)
    drop(l);

    // ---- `quadra dev`: written back ----
    let mut l = Local::open_dev(&root, &url, 64).unwrap();
    let cockpit = app_named(&mut l, admin, "Sales cockpit");
    assert_eq!(cockpit["read_only"], json!(false));
    let v = cockpit["version"].as_u64().unwrap();
    let mut cfg = cockpit["config"].clone();
    cfg["pages"][0]["name"] = json!("Overview");
    ok(
        &mut l,
        admin,
        "PUT",
        &format!("/v1/views/{id}"),
        json!({ "base": v, "name": "Sales overview", "config": cfg }),
    );
    let text = std::fs::read_to_string(root.join("interfaces/sales/cockpit.qdi")).unwrap();
    assert!(text.starts_with("app \"Sales overview\":"), "{text}");
    assert!(text.contains("page cockpit \"Overview\":"), "{text}");
    // the file checks, and reading it again changes nothing
    assert!(quadra_qdi::parse(&text).1.is_empty());
    assert_eq!(l.reload().map(|e| e["type"].clone()), Some(json!("deploy")));
    assert_eq!(
        app_named(&mut l, admin, "Sales overview")["version"]
            .as_u64()
            .unwrap(),
        v + 1
    );
    // a view deleted: its file without it
    let views = ok(
        &mut l,
        admin,
        "GET",
        "/v1/views?kind=view&entity=Order&scope=all",
        J::Null,
    )["views"]
        .clone();
    let pipeline = views
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == json!("Pipeline"))
        .unwrap()
        .clone();
    ok(
        &mut l,
        admin,
        "DELETE",
        &format!("/v1/views/{}?base={}", pipeline["id"], pipeline["version"]),
        J::Null,
    );
    let orders = std::fs::read_to_string(root.join("interfaces/sales/orders.qdi")).unwrap();
    assert!(
        orders.contains("view \"To ship\"") && !orders.contains("Pipeline"),
        "{orders}"
    );
    // a file edited on disk: installed again
    std::fs::write(
        root.join("interfaces/sales/orders.qdi"),
        orders.replace("view \"To ship\"", "view \"To ship soon\""),
    )
    .unwrap();
    assert!(l.reload().is_some());
    let views = ok(
        &mut l,
        admin,
        "GET",
        "/v1/views?kind=view&entity=Order&scope=all",
        J::Null,
    )["views"]
        .clone();
    let names: Vec<&str> = views
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"To ship soon") && !names.contains(&"To ship"),
        "{names:?}"
    );
    // a file that does not check is not applied: the last version keeps running
    std::fs::write(
        root.join("interfaces/sales/orders.qdi"),
        "view \"Broken\" of Order:\n    fields: nope\n",
    )
    .unwrap();
    let ev = l.reload().unwrap();
    assert_eq!(ev["type"], json!("problems"), "{ev}");
    assert!(ev.to_string().contains("no field `nope`"), "{ev}");
    let views = ok(
        &mut l,
        admin,
        "GET",
        "/v1/views?kind=view&entity=Order&scope=all",
        J::Null,
    )["views"]
        .clone();
    assert!(views
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["name"] == json!("To ship soon")));
    // a file removed: its app goes
    std::fs::remove_file(root.join("interfaces/sales/orders.qdi")).unwrap();
    std::fs::remove_file(root.join("interfaces/sales/desk.qdi")).unwrap();
    assert!(l.reload().is_some());
    let apps = ok(&mut l, admin, "GET", "/v1/views?kind=app", J::Null)["views"].clone();
    assert!(
        !apps
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == json!("Order desk")),
        "{apps}"
    );
    drop(l);

    // ---- production refuses a version whose interface files do not check ----
    std::fs::write(
        root.join("interfaces/broken.qdi"),
        "view \"Broken\" of Order:\n    fields: nope\n",
    )
    .unwrap();
    let err = Local::open(&root, &url, 64).err().expect("refused");
    assert!(err.contains("interface files"), "{err}");
    cleanup(target, &url, &root);
}

#[test]
fn interfaces_on_sqlite() {
    interfaces_on("sqlite");
}

#[test]
fn interfaces_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    interfaces_on(&pg);
}

/// `quadra apps pull|push|new` on SQLite.
#[test]
fn apps_commands() {
    let (root, url) = world("sqlite", "c");
    let user = |login: &str| {
        pick(
            &url,
            &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"),
        )
    };
    let s1 = user("sales0001");
    // a folder and an app made in the client, shared with a user and with a role
    {
        let mut l = Local::open(&root, &url, 64).unwrap();
        let f = ok(
            &mut l,
            s1,
            "POST",
            "/v1/views",
            json!({ "kind": "folder", "name": "Mine" }),
        )["view"]["id"]
            .as_u64()
            .unwrap();
        let cfg = json!({ "version": 1, "pages": [{ "id": "p1", "name": "Page 1", "components": [{ "id": "t", "type": "markdown", "layout": { "x": 0, "y": 0, "w": 24, "h": 2 }, "config": { "text": "Hello" }, "bindings": {} }] }] });
        ok(
            &mut l,
            s1,
            "POST",
            "/v1/views",
            json!({ "kind": "app", "name": "My board", "parent": f, "config": cfg, "shares": [{ "type": "role", "name": "Salesperson", "edit": false }, { "type": "user", "id": user("sales0002"), "edit": false }] }),
        );
    }
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
            .args(args)
            .arg(&root)
            .args(["--url", &url])
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    let (okay, out) = run(&["apps", "pull", "My board"]);
    assert!(okay, "{out}");
    assert!(
        out.contains("not written in the file"),
        "the user share: {out}"
    );
    let file = root.join("interfaces/Mine/my-board.qdi");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with("app \"My board\":") && text.contains("share: role Salesperson use"),
        "{text}"
    );
    // the data copy is archived; installed again from the file, in the same folder
    let (okay, out) = run(&["apps", "push"]);
    assert!(okay, "{out}");
    let mut l = Local::open(&root, &url, 64).unwrap();
    let b = ok(&mut l, s1, "GET", "/v1/views?kind=app", J::Null);
    let mine: Vec<&J> = b["views"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["name"] == json!("My board"))
        .collect();
    assert_eq!(mine.len(), 1, "{b}");
    assert_eq!(mine[0]["file"], json!("interfaces/Mine/my-board.qdi"));
    let folder = b["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("Mine"))
        .unwrap();
    assert_eq!(mine[0]["parent"], folder["id"]);
    drop(l);
    // a new file to start from, that checks
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
        .args(["apps", "new", "Warehouse board", "--entity", "Shipment"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
        .args(["check", "--json"])
        .arg(&root)
        .output()
        .unwrap();
    let j: J = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["ok"], json!(true), "{j}");
    cleanup("sqlite", &url, &root);
}
