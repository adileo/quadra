//! Saved views, boards and teams (engine/views.rs) over the engine of the server
//! (`quadra_server::Local`): who sees a view (private, users, teams, roles with inherited
//! roles, everyone; admins), that a view never widens what a user reads, concurrent edits
//! refused (409), expressions checked when saved, a board's columns as queries, moves of a
//! board through transitions (permission, `require`, frozen refused), and a deploy that renames
//! fields (views rewritten, recorded) or removes one (views marked broken, never deleted).
//! On `examples/stress` (the `tiny` world of `quadra bench gen`), on SQLite and, when
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

/// A copy of examples/stress without its tests (a rename changes the names they use).
fn project_copy(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir = std::env::temp_dir().join(format!("quadra-views-{}-{tag}", std::process::id()));
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
        let p = std::env::temp_dir().join(format!("quadra-views-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qv_{}_{tag}", std::process::id());
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
    let dir = std::env::temp_dir().join(format!("quadra-views-{}-{tag}-gen", std::process::id()));
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

    /// The ids of the views a user lists for an entity.
    fn listed(&mut self, user: u64, entity: &str) -> Vec<u64> {
        let b = self.ok(user, "GET", &format!("/v1/views?entity={entity}"), J::Null);
        b["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_u64().unwrap())
            .collect()
    }

    fn sees(&mut self, user: u64, entity: &str, id: u64) -> bool {
        let listed = self.listed(user, entity).contains(&id);
        let (s, _) = self.call(user, "GET", &format!("/v1/views/{id}"), J::Null);
        // listed exactly when it can be opened (admins open every view without listing it)
        if listed {
            assert_eq!(s, 200);
        }
        listed
    }

    fn query_ids(&mut self, user: u64, q: &J) -> (Vec<u64>, J) {
        let (s, b) = self.local.query(user, q);
        assert_eq!(s, 200, "{q} as #{user}: {b}");
        (
            b["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_u64().unwrap())
                .collect(),
            b,
        )
    }

    fn tx(&mut self, user: u64, ops: J, key: &str) -> (u16, J) {
        let base = self
            .local
            .query(
                user,
                &json!({ "entity": "Region", "fields": [], "limit": 1 }),
            )
            .1["base"]
            .as_u64()
            .unwrap();
        self.local
            .tx(user, &json!({ "base": base, "ops": ops }), key)
    }
}

fn views_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| {
        pick(
            &url,
            &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"),
        )
    };
    let (admin, s1, s2, s3, manager, logistics, controller, auditor) = (
        user("admin"),
        user("sales0001"),
        user("sales0002"),
        user("sales0003"),
        user("manager01"),
        user("logistics01"),
        user("controller"),
        user("auditor"),
    );
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };

    // ---- a private board, checked when saved ----
    let config = json!({
        "fields": ["number", "customer", "state", "total", "date"],
        "where": "total >= 0 EUR",
        "order": [{ "field": "date", "desc": true }],
        "columns": { "big": "total > 1000 EUR" },
        "colors": [{ "expr": "priority is High", "color": "#fee2e2" }],
        "kanban": { "field": "state", "cards": ["customer", "total"], "title": "number" },
    });
    let created = t.ok(
        s1,
        "POST",
        "/v1/views",
        json!({ "entity": "Order", "type": "kanban", "name": "My board", "config": config }),
    );
    let v = &created["view"];
    let id = v["id"].as_u64().unwrap();
    assert_eq!(
        (
            v["access"].clone(),
            v["version"].clone(),
            v["broken"].clone()
        ),
        (json!("owner"), json!(1), J::Null),
        "{v}"
    );
    for (bad, what) in [
        (json!({ "where": "totl > 3" }), "unknown name `totl`"),
        (json!({ "where": "total + 3" }), "cannot add"),
        (json!({ "where": "total" }), "must be yes or no"),
        (
            json!({ "fields": ["customer.nme"] }),
            "`Customer` has no field `nme`",
        ),
        (
            json!({ "colors": [{ "expr": "1\n    action evil:", "color": "red" }] }),
            "one line",
        ),
        (
            json!({ "kanban": { "field": "state", "cards": ["lines.qty"] } }),
            "not a record",
        ),
    ] {
        let (s, e) = t.call(
            s1,
            "POST",
            "/v1/views",
            json!({ "entity": "Order", "name": "Bad", "config": bad }),
        );
        assert_eq!(s, 400, "{bad}: {e}");
        assert!(e.to_string().contains(what), "{bad}: {e}");
    }
    let (s, _) = t.call(
        logistics,
        "POST",
        "/v1/views",
        json!({ "entity": "Order", "name": "Not readable", "config": {} }),
    );
    assert_eq!(s, 403, "a view of an entity the user may not read");

    // ---- private: only its owner lists it; an admin opens and changes it, lists it with scope=all ----
    assert!(t.sees(s1, "Order", id));
    assert!(!t.sees(s2, "Order", id));
    assert_eq!(
        t.call(s2, "GET", &format!("/v1/views/{id}"), J::Null).0,
        404
    );
    assert!(
        !t.sees(admin, "Order", id),
        "private views are not listed to admins"
    );
    assert_eq!(
        t.ok(admin, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["access"],
        json!("admin")
    );
    let all = t.ok(admin, "GET", "/v1/views?entity=Order&scope=all", J::Null);
    assert!(all["views"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == json!(id)));
    assert_eq!(
        t.call(s1, "GET", "/v1/views?entity=Order&scope=all", J::Null)
            .0,
        403
    );

    // ---- shared with a user (use), then a role (inherited roles), then a team, then everyone ----
    let share = |t: &mut T, by: u64, shares: J| {
        let base = t.ok(by, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["version"]
            .as_u64()
            .unwrap();
        t.call(
            by,
            "PUT",
            &format!("/v1/views/{id}/shares"),
            json!({ "base": base, "shares": shares }),
        )
    };
    assert_eq!(
        share(&mut t, s1, json!([{ "type": "user", "id": s2 }])).0,
        200
    );
    assert!(t.sees(s2, "Order", id) && !t.sees(s3, "Order", id));
    assert_eq!(
        t.ok(s2, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["access"],
        json!("use")
    );
    let v2 = t.ok(s2, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["version"]
        .as_u64()
        .unwrap();
    assert_eq!(
        t.call(
            s2,
            "PUT",
            &format!("/v1/views/{id}"),
            json!({ "base": v2, "name": "Mine now" })
        )
        .0,
        403,
        "a user of a view does not change it"
    );
    assert_eq!(
        share(&mut t, s2, json!([{ "type": "everyone" }])).0,
        403,
        "only the owner shares"
    );
    assert_eq!(
        share(
            &mut t,
            s1,
            json!([{ "type": "role", "name": "Salesperson" }])
        )
        .0,
        200
    );
    // manager01 is a RegionalManager, which extends Salesperson
    assert!(t.sees(s3, "Order", id) && t.sees(manager, "Order", id));
    assert!(!t.sees(controller, "Order", id) && !t.sees(auditor, "Order", id));
    assert_eq!(
        share(&mut t, s1, json!([{ "type": "role", "name": "Nobody" }])).0,
        400
    );

    // teams: admins create them; their owners change them
    let (s, _) = t.call(s1, "POST", "/v1/teams", json!({ "name": "North" }));
    assert_eq!(s, 403, "only admins create teams");
    let team = t.ok(
        admin,
        "POST",
        "/v1/teams",
        json!({ "name": "North", "members": [{ "id": s3, "owner": true }, { "id": controller }] }),
    )["team"]
        .clone();
    let team_id = team["id"].as_u64().unwrap();
    assert_eq!(
        t.call(admin, "POST", "/v1/teams", json!({ "name": "north" }))
            .0,
        409,
        "team names are unique"
    );
    assert_eq!(
        share(&mut t, s1, json!([{ "type": "team", "id": team_id }])).0,
        200
    );
    assert!(t.sees(controller, "Order", id) && t.sees(s3, "Order", id));
    assert!(
        !t.sees(s2, "Order", id) && !t.sees(manager, "Order", id) && !t.sees(auditor, "Order", id)
    );
    // a member who is not an owner does not change the team; its owner does
    let tv = team["version"].as_u64().unwrap();
    assert_eq!(
        t.call(
            controller,
            "PUT",
            &format!("/v1/teams/{team_id}"),
            json!({ "base": tv, "members": [] })
        )
        .0,
        403
    );
    let t2 = t.ok(
        s3,
        "PUT",
        &format!("/v1/teams/{team_id}"),
        json!({ "base": tv, "members": [{ "id": s3, "owner": true }] }),
    )["team"]
        .clone();
    assert!(
        !t.sees(controller, "Order", id),
        "removed from the team: the view is gone for them"
    );
    // the same old version again: refused with who changed what
    let (s, c) = t.call(
        admin,
        "PUT",
        &format!("/v1/teams/{team_id}"),
        json!({ "base": tv, "name": "South" }),
    );
    assert_eq!(s, 409, "{c}");
    assert_eq!(c["conflicts"][0]["user"], json!("sales0003"));
    let hist = t.ok(
        admin,
        "GET",
        &format!("/v1/teams/{team_id}/history"),
        J::Null,
    );
    assert_eq!(hist["entries"].as_array().unwrap().len(), 2, "{hist}");
    // a deleted team gives no access
    t.ok(admin, "PUT", &format!("/v1/teams/{team_id}"), json!({ "base": t2["version"], "members": [{ "id": s3, "owner": true }, { "id": controller }] }));
    assert!(t.sees(controller, "Order", id));
    let tv = t.ok(admin, "GET", &format!("/v1/teams/{team_id}"), J::Null)["team"]["version"]
        .as_u64()
        .unwrap();
    t.ok(
        admin,
        "DELETE",
        &format!("/v1/teams/{team_id}?base={tv}"),
        J::Null,
    );
    assert!(!t.sees(controller, "Order", id));

    assert_eq!(
        share(
            &mut t,
            s1,
            json!([{ "type": "everyone" }, { "type": "user", "id": s2, "edit": true }])
        )
        .0,
        200
    );
    assert!(t.sees(auditor, "Order", id) && t.sees(controller, "Order", id));
    assert!(
        !t.sees(logistics, "Order", id),
        "a view of an entity the user may not read is not listed"
    );

    // ---- concurrent edits: the second from the same version is refused ----
    let base = t.ok(s2, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["version"]
        .as_u64()
        .unwrap();
    let mut c2 = config.clone();
    c2["where"] = json!("total >= 1 EUR");
    let after = t.ok(
        s2,
        "PUT",
        &format!("/v1/views/{id}"),
        json!({ "base": base, "config": c2 }),
    )["view"]
        .clone();
    assert_eq!(
        (after["access"].clone(), after["version"].as_u64()),
        (json!("edit"), Some(base + 1)),
        "a co-owner changes it"
    );
    let (s, c) = t.call(
        s1,
        "PUT",
        &format!("/v1/views/{id}"),
        json!({ "base": base, "name": "Renamed" }),
    );
    assert_eq!(s, 409, "{c}");
    assert_eq!(c["conflicts"][0]["user"], json!("sales0002"));
    assert!(
        c["conflicts"][0]["what"]
            .to_string()
            .contains("config.where"),
        "{c}"
    );
    let (s, e) = t.call(
        s1,
        "PUT",
        &format!("/v1/views/{id}"),
        json!({ "base": base + 1, "config": { "where": "nope" } }),
    );
    assert_eq!(s, 400, "{e}");
    let hist = t.ok(s1, "GET", &format!("/v1/views/{id}/history"), J::Null);
    let actions: Vec<&str> = hist["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions.first(), Some(&"update"));
    assert_eq!(actions.last(), Some(&"create"));
    // favourites and defaults are per user
    t.ok(
        s2,
        "PUT",
        &format!("/v1/views/{id}/prefs"),
        json!({ "favourite": true, "default": true }),
    );
    assert_eq!(
        t.ok(s2, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["default"],
        json!(true)
    );
    assert_eq!(
        t.ok(s1, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["default"],
        json!(false)
    );

    // ---- a view never widens what a user reads: its query gives each user their own rows ----
    let view = t.ok(auditor, "GET", &format!("/v1/views/{id}"), J::Null)["view"]["config"].clone();
    let q = json!({ "entity": "Order", "fields": view["fields"], "where": view["where"], "order_by": view["order"], "limit": 1000 });
    let (mine, r1) = t.query_ids(s1, &q);
    let (all_rows, r2) = t.query_ids(admin, &q);
    let direct = json!({ "entity": "Order", "fields": ["number"], "where": "total >= 1 EUR", "order_by": [{ "field": "date", "desc": true }], "limit": 1000 });
    assert_eq!(mine, t.query_ids(s1, &direct).0);
    assert_eq!(all_rows, t.query_ids(admin, &direct).0);
    assert!(
        r1["total"].as_u64() < r2["total"].as_u64(),
        "the salesperson reads fewer orders than the admin"
    );
    // the logistics user may not read orders at all, view or not
    assert_eq!(t.local.query(logistics, &q).0, 403);

    // ---- a board: each column a query of the view's filter and that state; counts from one group query ----
    for who in [s1, manager, admin] {
        let groups = t.local.query(who, &json!({ "entity": "Order", "where": view["where"], "group_by": ["state"], "aggregates": { "total": "sum" }, "limit": 100 })).1;
        assert_eq!(groups["sql"], json!(true), "{groups}");
        for state in ["Draft", "Confirmed", "Shipped", "Cancelled"] {
            let column = json!({ "entity": "Order", "fields": ["number", "customer", "total"], "where": view["where"], "group_filter": [{ "field": "state", "value": state }], "order_by": view["order"], "limit": 1000 });
            let direct = json!({ "entity": "Order", "fields": [], "where": format!("(total >= 1 EUR) and state is {state}"), "order_by": view["order"], "limit": 1000 });
            let (a, ra) = t.query_ids(who, &column);
            let (b, _) = t.query_ids(who, &direct);
            assert_eq!(a, b, "column {state} as #{who}");
            let count = groups["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["keys"][0]["value"] == json!(state))
                .map_or(0, |g| g["count"].as_u64().unwrap());
            assert_eq!(
                Some(count),
                ra["total"].as_u64(),
                "the count of column {state} as #{who}"
            );
            // the first page and the next ones (the cursor) give the same rows
            let mut paged = Vec::new();
            let mut cq = column.clone();
            cq["limit"] = json!(7);
            loop {
                let (ids, r) = t.query_ids(who, &cq);
                paged.extend(ids);
                match r["next"].as_str() {
                    Some(n) => {
                        cq["after"] = json!(n);
                        cq["count"] = json!(false);
                    }
                    None => break,
                }
            }
            assert_eq!(paged, a, "column {state} in pages as #{who}");
        }
    }

    // ---- moves on the board: a transition (its action) through a transaction ----
    let draft = pick(&url, &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" = {s1} AND o.\"state\" = 'Draft' AND o.\"line_count\" > 0 AND c.\"active\" AND NOT r.\"locked\" ORDER BY o.\"id\" LIMIT 1"));
    let (s, b) = t.tx(
        s1,
        json!([{ "op": "action", "entity": "Order", "id": draft, "action": "confirm" }]),
        "move-1",
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        t.local.record(s1, "Order", draft).1["cells"]["state"]["d"],
        json!("Confirmed")
    );
    // not allowed to the salesperson: Confirmed -> Shipped is `ship`, a manager's action
    let (s, b) = t.tx(
        s1,
        json!([{ "op": "action", "entity": "Order", "id": draft, "action": "ship" }]),
        "move-2",
    );
    assert_eq!(s, 403, "{b}");
    // the record's actions say so before the drop (the board disables those columns)
    let actions = t.local.record(s1, "Order", draft).1["actions"].clone();
    assert!(
        actions
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == json!("cancel") && a["to"] == json!("Cancelled")),
        "{actions}"
    );
    assert!(!actions.to_string().contains("\"ship\""), "{actions}");
    // a `require` of the action refuses it: an order without lines
    let customer = pick(&url, &format!("SELECT CAST(c.\"id\" AS TEXT) FROM \"customer\" c JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" = {s1} AND c.\"active\" AND r.\"active\" AND NOT r.\"locked\" ORDER BY c.\"id\" LIMIT 1"));
    let (s, b) = t.tx(s1, json!([{ "op": "create", "entity": "Order", "ref": "o", "values": { "number": "VIEWS-1", "customer": customer, "date": "2026-11-20" } }]), "move-3");
    assert_eq!(s, 200, "{b}");
    let empty = b["created"]["o"].as_u64().unwrap();
    let (s, b) = t.tx(
        s1,
        json!([{ "op": "action", "entity": "Order", "id": empty, "action": "confirm" }]),
        "move-4",
    );
    assert_eq!(s, 422, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .contains("The order has no lines"),
        "{b}"
    );
    // a frozen (cancelled) order: no move of a plain field, no transition from its state
    let frozen = pick(&url, "SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" = 'Cancelled' AND \"_frozen_tx\" IS NOT NULL ORDER BY \"id\" LIMIT 1");
    let (s, b) = t.tx(admin, json!([{ "op": "update", "entity": "Order", "id": frozen, "values": { "priority": "High" } }]), "move-5");
    assert_eq!(s, 422, "{b}");
    assert!(b["error"].as_str().unwrap().contains("frozen"), "{b}");
    let (s, b) = t.tx(
        admin,
        json!([{ "op": "action", "entity": "Order", "id": frozen, "action": "confirm" }]),
        "move-6",
    );
    assert_eq!(s, 422, "{b}");
    // a plain choice moves by an update
    let (s, b) = t.tx(admin, json!([{ "op": "update", "entity": "Order", "id": draft, "values": { "priority": "High" } }]), "move-7");
    assert_eq!(s, 200, "{b}");

    // ---- a deploy: renamed fields are renamed in the views, a removed one breaks them ----
    let order_view = t.ok(s1, "POST", "/v1/views", json!({ "entity": "Order", "name": "By customer", "config": {
        "fields": ["number", "customer.name", "total"], "widths": { "customer.name": 220 },
        "where": "customer.name.starts_with(\"A\") or total > 0 EUR",
        "columns": { "cn": "customer.name" }, "order": [{ "field": "customer.name", "desc": false }],
        "group": { "keys": [{ "field": "customer.name" }], "aggregates": { "total": "sum" }, "pivot": false, "order": { "by": "key", "desc": false }, "measure": "count" },
    } }))["view"]["id"].as_u64().unwrap();
    let customer_view = t.ok(s1, "POST", "/v1/views", json!({ "entity": "Customer", "name": "Named", "config": {
        "fields": ["code", "name"], "where": "name != \"\" and orders.any(o => o.customer.name != \"\")", "known": ["code", "name", "region"],
    } }))["view"]["id"].as_u64().unwrap();
    let region_view = t.ok(admin, "POST", "/v1/views", json!({ "entity": "Region", "name": "Targets", "config": { "fields": ["code", "target"], "where": "target > 0 EUR" } }))["view"]["id"].as_u64().unwrap();
    drop(t);
    let sales = root.join("sales.qd");
    let text = std::fs::read_to_string(&sales).unwrap();
    let text = text
        .replacen(
            "    code: Code unique\n    name: Text\n    region: Region",
            "    code: Code unique\n    title: Text\n    region: Region",
            1,
        )
        .replacen("    target: Money<EUR> default 0.00 EUR\n", "", 1);
    std::fs::write(&sales, text).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(root.join("migrations/titles.qd"), "migration titles:\n    reason: \"Customers have a title\"\n    rename Customer.name -> Customer.title\n").unwrap();
    let checked = quadra_analysis::check(&quadra_sema::Project::load(&root).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    quadra_store::ops::deploy(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        &root,
        clock(),
        false,
        "test deploy",
    )
    .unwrap();
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    let v = t.ok(s1, "GET", &format!("/v1/views/{order_view}"), J::Null)["view"].clone();
    let c = &v["config"];
    assert_eq!(v["broken"], J::Null, "{v}");
    assert_eq!(c["fields"], json!(["number", "customer.title", "total"]));
    assert_eq!(
        c["where"],
        json!("customer.title.starts_with(\"A\") or total > 0 EUR")
    );
    assert_eq!(c["columns"]["cn"], json!("customer.title"));
    assert_eq!(c["order"][0]["field"], json!("customer.title"));
    assert_eq!(c["group"]["keys"][0]["field"], json!("customer.title"));
    assert_eq!(c["widths"]["customer.title"], json!(220));
    let hist = t.ok(
        s1,
        "GET",
        &format!("/v1/views/{order_view}/history"),
        J::Null,
    );
    let last = &hist["entries"][0];
    assert!(
        last["action"]
            .as_str()
            .unwrap()
            .starts_with("deploy: renamed"),
        "{hist}"
    );
    assert_eq!(last["user"], J::Null, "the deploy changed it, not a user");
    let v = t.ok(s1, "GET", &format!("/v1/views/{customer_view}"), J::Null)["view"].clone();
    assert_eq!(
        v["config"]["where"],
        json!("title != \"\" and orders.any(o => o.customer.title != \"\")"),
        "{v}"
    );
    assert_eq!(v["config"]["fields"], json!(["code", "title"]));
    assert_eq!(v["config"]["known"], json!(["code", "title", "region"]));
    // the removed field: broken, with the checker's message, still there
    let v = t.ok(admin, "GET", &format!("/v1/views/{region_view}"), J::Null)["view"].clone();
    let broken = v["broken"].to_string();
    assert!(broken.contains("target"), "{v}");
    assert!(t.listed(admin, "Region").contains(&region_view));
    // the views still run as their (rewritten) queries
    let q = json!({ "entity": "Order", "fields": ["number", "customer.title"], "where": "customer.title.starts_with(\"A\") or total > 0 EUR", "limit": 5 });
    t.query_ids(s1, &q);
    // fixed by its owner: no longer broken
    let base = v["version"].as_u64().unwrap();
    let fixed = t.ok(
        admin,
        "PUT",
        &format!("/v1/views/{region_view}"),
        json!({ "base": base, "config": { "fields": ["code"], "where": "active" } }),
    )["view"]
        .clone();
    assert_eq!(fixed["broken"], J::Null);
    // opened again on the same model: nothing changes
    drop(t);
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    assert_eq!(
        t.ok(
            s1,
            "GET",
            &format!("/v1/views/{order_view}/history"),
            J::Null
        )["entries"]
            .as_array()
            .unwrap()
            .len(),
        hist["entries"].as_array().unwrap().len()
    );

    // ---- export and import (users by login, teams by name) ----
    let file = std::env::temp_dir().join(format!("quadra-views-{}-{tag}.json", std::process::id()));
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
    };
    let before = t.listed(s1, "Order").len();
    drop(t);
    run(&["db", "views", "export", file.to_str().unwrap()]);
    let doc: J = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(
        doc["views"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["owner"] == json!("sales0001") && v["name"] == json!("My board")),
        "{doc}"
    );
    run(&["db", "views", "import", file.to_str().unwrap()]);
    let _ = std::fs::remove_file(&file);
    let mut t = T {
        local: Local::open(&root, &url, 64).unwrap(),
    };
    assert_eq!(
        t.listed(s1, "Order").len(),
        2 * before,
        "every view of sales0001 imported again"
    );
    drop(t);
    cleanup(target, &url, &root);
}

#[test]
fn views_on_sqlite() {
    views_on("sqlite");
}

#[test]
fn views_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    views_on(&pg);
}
