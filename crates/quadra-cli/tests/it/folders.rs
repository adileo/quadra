//! Folders of apps (engine/views.rs, `kind = "folder"`) over the engine of the server
//! (`quadra_server::Local`): a tree of folders and apps, who is shown which folder (owner,
//! shared, holding something shown), sharing a folder never sharing what it holds, moves
//! (cycles, depth, names unique next to each other, the right to put things in a folder),
//! deleting a folder (what it holds moves up), 409 on a stale `base`, the history, the export
//! and import of the tree, and a database of system tables version 6 upgraded on open. On
//! `examples/stress` (the `tiny` world of `quadra bench gen`), on SQLite and, when
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

fn project_copy(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir = std::env::temp_dir().join(format!("quadra-folders-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml")) {
            std::fs::copy(&p, dir.join(e.file_name())).unwrap();
        }
    }
    dir
}

fn fresh(target: &str, tag: &str) -> String {
    if target == "sqlite" {
        let p =
            std::env::temp_dir().join(format!("quadra-folders-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qf_{}_{tag}", std::process::id());
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
    let dir = std::env::temp_dir().join(format!("quadra-folders-{}-{tag}-gen", std::process::id()));
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

struct T {
    local: Local,
}

impl T {
    fn call(&mut self, user: u64, method: &str, path: &str, body: J) -> (u16, J) {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let q: Vec<(&str, &str)> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|p| p.split_once('='))
            .collect();
        self.local.views(user, method, path, &q, &body)
    }

    fn ok(&mut self, user: u64, method: &str, path: &str, body: J) -> J {
        let (s, b) = self.call(user, method, path, body);
        assert!(s == 200 || s == 201, "{method} {path} as #{user}: {s} {b}");
        b
    }

    fn folder(&mut self, user: u64, name: &str, parent: Option<u64>) -> u64 {
        self.ok(
            user,
            "POST",
            "/v1/views",
            json!({ "kind": "folder", "name": name, "parent": parent }),
        )["view"]["id"]
            .as_u64()
            .unwrap()
    }

    fn app(&mut self, user: u64, name: &str, parent: Option<u64>) -> u64 {
        self.ok(
            user,
            "POST",
            "/v1/views",
            json!({ "kind": "app", "name": name, "parent": parent }),
        )["view"]["id"]
            .as_u64()
            .unwrap()
    }

    /// What `user` is shown: (apps: id → parent, folders: id → (parent, implied)).
    fn tree(&mut self, user: u64) -> (Vec<(u64, Option<u64>)>, Vec<(u64, Option<u64>, bool)>) {
        let b = self.ok(user, "GET", "/v1/views?kind=app", J::Null);
        let apps = b["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["id"].as_u64().unwrap(), v["parent"].as_u64()))
            .collect();
        let folders = b["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["id"].as_u64().unwrap(),
                    v["parent"].as_u64(),
                    v["implied"].as_bool().unwrap(),
                )
            })
            .collect();
        (apps, folders)
    }

    fn folder_ids(&mut self, user: u64) -> Vec<u64> {
        self.tree(user).1.iter().map(|f| f.0).collect()
    }

    fn get(&mut self, user: u64, id: u64) -> J {
        self.ok(user, "GET", &format!("/v1/views/{id}"), J::Null)["view"].clone()
    }

    fn put(&mut self, user: u64, id: u64, base: u64, body: J) -> (u16, J) {
        let mut b = body;
        b["base"] = json!(base);
        self.call(user, "PUT", &format!("/v1/views/{id}"), b)
    }

    fn share(&mut self, user: u64, id: u64, shares: J) {
        let base = self.get(user, id)["version"].as_u64().unwrap();
        self.ok(
            user,
            "PUT",
            &format!("/v1/views/{id}/shares"),
            json!({ "base": base, "shares": shares }),
        );
    }
}

fn folders_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| {
        pick(
            &url,
            &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"),
        )
    };
    let (admin, s1, s2, s3) = (
        user("admin"),
        user("sales0001"),
        user("sales0002"),
        user("sales0003"),
    );
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };

    // ---- a tree: Sales / 2026 / Board ----
    let sales = t.folder(s1, "Sales", None);
    let y2026 = t.folder(s1, "2026", Some(sales));
    let board = t.app(s1, "Board", Some(y2026));
    let f = t.get(s1, sales);
    assert_eq!(
        (
            f["kind"].clone(),
            f["type"].clone(),
            f["parent"].clone(),
            f["access"].clone()
        ),
        (json!("folder"), json!("folder"), J::Null, json!("owner"))
    );
    assert_eq!(t.get(s1, board)["parent"], json!(y2026));
    let (apps, folders) = t.tree(s1);
    assert!(apps.contains(&(board, Some(y2026))), "{apps:?}");
    assert!(
        folders.contains(&(sales, None, false)) && folders.contains(&(y2026, Some(sales), false)),
        "{folders:?}"
    );
    // `kind=folder` lists them on their own
    let only = t.ok(s1, "GET", "/v1/views?kind=folder", J::Null);
    assert_eq!(only["views"].as_array().unwrap().len(), 2);
    // a folder has no configuration; a view of an entity is in no folder
    assert_eq!(
        t.call(
            s1,
            "POST",
            "/v1/views",
            json!({ "kind": "folder", "name": "X", "config": {} })
        )
        .0,
        400
    );
    assert_eq!(
        t.call(
            s1,
            "POST",
            "/v1/views",
            json!({ "entity": "Order", "name": "V", "parent": sales })
        )
        .0,
        400
    );
    let view = t.ok(
        s1,
        "POST",
        "/v1/views",
        json!({ "entity": "Order", "name": "V" }),
    )["view"]
        .clone();
    assert_eq!(
        t.put(
            s1,
            view["id"].as_u64().unwrap(),
            view["version"].as_u64().unwrap(),
            json!({ "parent": sales })
        )
        .0,
        400
    );

    // ---- names: valid directory names, unique next to each other (any case) ----
    for bad in ["a/b", "a\\b", "..", "x.", "what?", "a:b"] {
        assert_eq!(
            t.call(
                s1,
                "POST",
                "/v1/views",
                json!({ "kind": "folder", "name": bad })
            )
            .0,
            400,
            "{bad}"
        );
    }
    let (s, b) = t.call(
        s1,
        "POST",
        "/v1/views",
        json!({ "kind": "folder", "name": "sales" }),
    );
    assert_eq!(s, 409, "{b}");
    assert!(b["error"].as_str().unwrap().contains("at the top"), "{b}");
    // whoever owns the other one: the tree is one for everyone
    assert_eq!(
        t.call(
            s2,
            "POST",
            "/v1/views",
            json!({ "kind": "folder", "name": "SALES" })
        )
        .0,
        409
    );
    let inner = t.folder(s1, "Sales", Some(y2026));
    let y2025 = t.folder(s1, "2025", Some(sales));
    let v2025 = t.get(s1, y2025)["version"].as_u64().unwrap();
    assert_eq!(
        t.put(s1, y2025, v2025, json!({ "name": "2026" })).0,
        409,
        "a sibling is named so"
    );
    assert_eq!(t.put(s1, y2025, v2025, json!({ "name": "a/b" })).0, 400);
    let renamed = t.put(s1, y2025, v2025, json!({ "name": "Old" })).1["view"].clone();
    assert_eq!(renamed["name"], json!("Old"));

    // ---- who is shown what ----
    let (apps, folders) = t.tree(s2);
    assert!(
        apps.is_empty() && folders.is_empty(),
        "private: {apps:?} {folders:?}"
    );
    t.share(s1, board, json!([{ "type": "user", "id": s2 }]));
    let (apps, folders) = t.tree(s2);
    assert_eq!(apps, vec![(board, Some(y2026))]);
    // the folders that hold it, shown for it (implied); not the others
    let mut fs = folders.clone();
    fs.sort();
    let mut want = vec![(sales, None, true), (y2026, Some(sales), true)];
    want.sort();
    assert_eq!(fs, want);
    // shown for what it holds: not opened, nothing put in it
    assert_eq!(
        t.call(s2, "GET", &format!("/v1/views/{sales}"), J::Null).0,
        404
    );
    let (s, b) = t.call(
        s2,
        "POST",
        "/v1/views",
        json!({ "kind": "app", "name": "Mine", "parent": sales }),
    );
    assert_eq!(s, 403, "{b}");
    // a folder neither shown nor shared: as if it did not exist
    assert_eq!(
        t.call(
            s2,
            "POST",
            "/v1/views",
            json!({ "kind": "app", "name": "Mine", "parent": y2025 })
        )
        .0,
        404
    );
    // shared to use: shown, still nothing put in it
    t.share(
        s1,
        sales,
        json!([{ "type": "user", "id": s2 }, { "type": "user", "id": s3 }]),
    );
    assert!(t.tree(s2).1.contains(&(sales, None, false)));
    assert_eq!(
        t.call(
            s2,
            "POST",
            "/v1/views",
            json!({ "kind": "app", "name": "Mine", "parent": sales })
        )
        .0,
        403
    );
    // sharing a folder does not share what it holds
    let (apps, folders) = t.tree(s3);
    assert!(apps.is_empty(), "{apps:?}");
    assert_eq!(folders, vec![(sales, None, false)]);
    // shared to change: things go in it; their owner keeps them private
    t.share(
        s1,
        sales,
        json!([{ "type": "user", "id": s2, "edit": true }]),
    );
    let mine = t.app(s2, "Mine", Some(sales));
    assert!(
        !t.tree(s1).0.iter().any(|a| a.0 == mine),
        "an app put in my folder is not shared with me"
    );
    assert!(t.tree(s2).0.contains(&(mine, Some(sales))));
    // admins: every folder with scope=all
    let all = t.ok(admin, "GET", "/v1/views?kind=folder&scope=all", J::Null);
    assert_eq!(all["views"].as_array().unwrap().len(), 4);
    assert_eq!(
        t.call(s1, "GET", "/v1/views?kind=folder&scope=all", J::Null)
            .0,
        403
    );

    // ---- moves ----
    let vb = t.get(s1, board)["version"].as_u64().unwrap();
    let moved = t.put(s1, board, vb, json!({ "parent": null })).1["view"].clone();
    assert_eq!(
        (moved["parent"].clone(), moved["version"].as_u64().unwrap()),
        (J::Null, vb + 1)
    );
    // stale base: 409 with what changed
    let (s, b) = t.put(s1, board, vb, json!({ "parent": y2026 }));
    assert_eq!(s, 409);
    assert_eq!(b["conflicts"][0]["what"], json!(["parent"]), "{b}");
    t.put(s1, board, vb + 1, json!({ "parent": y2026 }));
    let hist = t.ok(s1, "GET", &format!("/v1/views/{board}/history"), J::Null)["entries"].clone();
    assert_eq!(hist[0]["changed"], json!(["parent"]));
    assert_eq!(hist[0]["state"]["parent"], json!(y2026));
    assert_eq!(hist[1]["state"]["parent"], J::Null);
    // someone it is shared with to use does not move it
    assert_eq!(t.put(s2, board, vb + 2, json!({ "parent": null })).0, 403);
    // a move while the app is being edited is not a conflict: the edit, saved on the version
    // it started from, lands in the folder the app was moved to (the client's "Move to
    // folder…" while the app is open with unsaved changes)
    let ve = t.get(s1, board)["version"].as_u64().unwrap();
    t.put(s1, board, ve, json!({ "parent": null }));
    let edited =
        json!({ "version": 1, "pages": [{ "id": "p1", "name": "Edited", "components": [] }] });
    let (s, b) = t.put(s1, board, ve, json!({ "config": edited }));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (
            b["view"]["parent"].clone(),
            b["view"]["config"]["pages"][0]["name"].clone()
        ),
        (J::Null, json!("Edited"))
    );
    // an edit of the configuration since is one
    let (s, b) = t.put(s1, board, ve, json!({ "config": edited }));
    assert_eq!(s, 409, "{b}");
    let ve = t.get(s1, board)["version"].as_u64().unwrap();
    t.put(s1, board, ve, json!({ "parent": y2026 }));
    // no cycles
    let vs = t.get(s1, sales)["version"].as_u64().unwrap();
    assert_eq!(
        t.put(s1, sales, vs, json!({ "parent": sales })).0,
        400,
        "into itself"
    );
    assert_eq!(
        t.put(s1, sales, vs, json!({ "parent": inner })).0,
        400,
        "into a folder it holds"
    );
    // the names next to it: `Sales` (inner) does not go to the top where `Sales` is
    let vi = t.get(s1, inner)["version"].as_u64().unwrap();
    assert_eq!(t.put(s1, inner, vi, json!({ "parent": null })).0, 409);
    // depth: 10 levels at most
    let mut chain = vec![t.folder(s1, "Deep", None)];
    for k in 2..=10 {
        let last = *chain.last().unwrap();
        chain.push(t.folder(s1, &format!("d{k}"), Some(last)));
    }
    assert_eq!(
        t.call(
            s1,
            "POST",
            "/v1/views",
            json!({ "kind": "folder", "name": "d11", "parent": chain[9] })
        )
        .0,
        400
    );
    // an app goes at any depth
    t.app(s1, "Deep app", Some(chain[9]));
    // `2026` holds `Sales`: two levels, under d9 (depth 9) it would reach 11
    let vy = t.get(s1, y2026)["version"].as_u64().unwrap();
    let (s, b) = t.put(s1, y2026, vy, json!({ "parent": chain[8] }));
    assert_eq!(s, 400, "{b}");
    assert!(b["error"].as_str().unwrap().contains("10 deep"), "{b}");
    let (s, b) = t.put(s1, y2026, vy, json!({ "parent": chain[7] }));
    assert_eq!(s, 200, "{b}");
    let vy = b["view"]["version"].as_u64().unwrap();
    assert_eq!(t.put(s1, y2026, vy, json!({ "parent": sales })).0, 200);
    // into a folder of someone else's not shared to change: refused
    let other = t.folder(s3, "Elsewhere", None);
    t.share(s3, other, json!([{ "type": "user", "id": s1 }]));
    let vb = t.get(s1, board)["version"].as_u64().unwrap();
    assert_eq!(t.put(s1, board, vb, json!({ "parent": other })).0, 403);
    assert_eq!(t.put(s1, board, vb, json!({ "parent": 999999 })).0, 404);

    // ---- the user's order inside a folder: the order of views ----
    let a2 = t.app(s1, "Another", Some(y2026));
    t.ok(s1, "PUT", "/v1/views/order", json!({ "ids": [a2, board] }));
    let order: Vec<u64> = t
        .tree(s1)
        .0
        .iter()
        .filter(|a| a.1 == Some(y2026))
        .map(|a| a.0)
        .collect();
    assert_eq!(order, vec![a2, board]);

    // ---- delete: what the folder holds moves up ----
    let vs = t.get(s1, sales)["version"].as_u64().unwrap();
    assert_eq!(
        t.call(
            s2,
            "DELETE",
            &format!("/v1/views/{sales}?base={vs}"),
            J::Null
        )
        .0,
        403,
        "co-owners do not delete"
    );
    assert_eq!(
        t.call(
            s1,
            "DELETE",
            &format!("/v1/views/{sales}?base={}", vs - 1),
            J::Null
        )
        .0,
        409
    );
    // a folder named as one it would move next to: refused until renamed
    let clash = t.folder(s1, "2026", None);
    let (s, b) = t.call(
        s1,
        "DELETE",
        &format!("/v1/views/{sales}?base={vs}"),
        J::Null,
    );
    assert_eq!(s, 409, "{b}");
    let vc = t.get(s1, clash)["version"].as_u64().unwrap();
    t.put(s1, clash, vc, json!({ "name": "2026 (old)" }));
    let (s, b) = t.call(
        s1,
        "DELETE",
        &format!("/v1/views/{sales}?base={vs}"),
        J::Null,
    );
    assert_eq!(s, 200, "{b}");
    // 2026, Old and s2's private app
    assert_eq!(
        (b["moved"].clone(), b["to"].clone()),
        (json!(3), J::Null),
        "{b}"
    );
    assert_eq!(t.get(s1, y2026)["parent"], J::Null);
    assert_eq!(t.get(s2, mine)["parent"], J::Null);
    let h = t.ok(s2, "GET", &format!("/v1/views/{mine}/history"), J::Null)["entries"].clone();
    assert_eq!(h[0]["action"], json!("move: its folder was deleted"));
    assert!(!t.folder_ids(s1).contains(&sales));
    assert_eq!(
        t.call(s1, "GET", &format!("/v1/views/{sales}"), J::Null).0,
        404
    );
    // its history stays
    let h = t.ok(admin, "GET", "/v1/views?kind=folder&scope=all", J::Null);
    assert!(!h["views"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == json!(sales)));
    let deleted: u64 = pick(&url, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_view_log\" WHERE \"object_id\" = {sales} AND \"action\" = 'delete'"));
    assert_eq!(deleted, 1);

    // ---- export and import: the tree with them ----
    let file =
        std::env::temp_dir().join(format!("quadra-folders-{}-{tag}.json", std::process::id()));
    let run = |args: &[&str]| {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_quadra"))
            .args(args)
            .arg(&root)
            .args(["--url", &url])
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let before = t.tree(s1);
    drop(t);
    run(&["db", "views", "export", file.to_str().unwrap()]);
    let doc: J = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let items = doc["views"].as_array().unwrap();
    assert!(
        items.iter().any(|v| v["kind"] == json!("folder")
            && v["name"] == json!("2026")
            && v["key"] == json!(y2026)
            && v["parent"] == J::Null),
        "{doc}"
    );
    assert!(items.iter().any(|v| v["kind"] == json!("app")
        && v["name"] == json!("Board")
        && v["parent"] == json!(y2026)));
    // the last folder of the chain is gone before the import: made again, in its place
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    let v10 = t.get(s1, chain[9])["version"].as_u64().unwrap();
    let deep_app = t
        .tree(s1)
        .0
        .iter()
        .find(|a| a.1 == Some(chain[9]))
        .unwrap()
        .0;
    let vd = t.get(s1, deep_app)["version"].as_u64().unwrap();
    t.put(s1, deep_app, vd, json!({ "parent": null }));
    t.ok(
        s1,
        "DELETE",
        &format!("/v1/views/{}?base={v10}", chain[9]),
        J::Null,
    );
    drop(t);
    let out = run(&["db", "views", "import", file.to_str().unwrap()]);
    assert!(out.contains("1 folders"), "{out}");
    let _ = std::fs::remove_file(&file);
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    let (apps, folders) = t.tree(s1);
    assert_eq!(
        folders.len(),
        before.1.len(),
        "folders are found by name in their place: {folders:?}"
    );
    let d10 = folders.iter().find(|f| f.1 == Some(chain[8])).unwrap().0;
    assert_ne!(d10, chain[9]);
    // every app again, in the same folder (the one of the chain: in the new one, the app
    // itself having moved to the top)
    for (id, parent) in &before.0 {
        let (want, n) = if *parent == Some(chain[9]) {
            (Some(d10), 1)
        } else {
            (*parent, 2)
        };
        assert!(
            apps.iter().filter(|a| a.1 == want).count() >= n,
            "app {id} imported again in {want:?}: {apps:?}"
        );
    }
    drop(t);

    // ---- a database of version 6 (no `parent_id`) is upgraded when opened ----
    let mut c = Conn::open(&url).unwrap();
    c.batch("ALTER TABLE \"_quadra_views\" DROP COLUMN \"parent_id\"")
        .unwrap();
    c.batch("UPDATE \"_quadra_meta\" SET \"value\" = '6' WHERE \"key\" = 'system'")
        .unwrap();
    assert!(c
        .query(
            "SELECT t.\"parent_id\" FROM \"_quadra_views\" t WHERE 1 = 0",
            &[]
        )
        .is_err());
    drop(c);
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    let system = Conn::open(&url)
        .unwrap()
        .query(
            "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'system'",
            &[],
        )
        .unwrap()[0][0]
        .clone();
    assert_eq!(system.as_deref(), Some("14"));
    assert!(
        Conn::open(&url)
            .unwrap()
            .query(
                "SELECT t.\"id\" FROM \"_quadra_login_codes\" t WHERE 1 = 0",
                &[]
            )
            .is_ok(),
        "the tables of later versions are added"
    );
    let (apps, folders) = t.tree(s1);
    assert!(
        apps.iter().all(|a| a.1.is_none()) && folders.iter().all(|f| f.1.is_none()),
        "everything at the top again"
    );
    let vb = t.get(s1, board)["version"].as_u64().unwrap();
    assert_eq!(
        t.put(s1, board, vb, json!({ "parent": y2026 })).1["view"]["parent"],
        json!(y2026)
    );
    drop(t);
    cleanup(target, &url, &root);
}

#[test]
fn folders_on_sqlite() {
    folders_on("sqlite");
}

#[test]
fn folders_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    folders_on(&pg);
}
