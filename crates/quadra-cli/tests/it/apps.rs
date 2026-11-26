//! Apps and their spreadsheets (engine/apps.rs, engine/sheets.rs, engine/xlsx.rs) over the
//! engine of the server (`quadra_server::Local`): apps shared like views (who sees, who
//! changes, 409, checked configurations), formulas (references, ranges, other sheets, cycles,
//! spills and `#SPILL`, type errors, `none`), formulas over the database giving each user what
//! a query gives them (row rules, hidden fields), values kept until a commit changes what they
//! read, `group_by` spilled, the Excel export read back, and time limits. On
//! `examples/stress` (the `tiny` world of `quadra bench gen`), on SQLite and, when
//! `QUADRA_TEST_PG` holds a url, on PostgreSQL.

use quadra_server::Local;
use quadra_store::conn::Conn;
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

fn project_copy(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir = std::env::temp_dir().join(format!("quadra-apps-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml")) {
            std::fs::copy(&p, dir.join(e.file_name())).unwrap();
        }
    }
    // a metric, for the certified mark
    std::fs::write(dir.join("metrics.qd"), "from sales import Order\n\nmetric confirmed_orders() -> Int:\n    label: \"Confirmed orders\"\n    value = Order.count(o => o.state is Confirmed)\n").unwrap();
    dir
}

fn fresh(target: &str, tag: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-apps-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qa_{}_{tag}", std::process::id());
        Conn::open(target).unwrap().batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")).unwrap();
        format!("{target}#schema={schema}")
    }
}

fn cleanup(target: &str, url: &str, root: &Path) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{s}"));
        }
    } else if let Some((_, schema)) = url.split_once("#schema=") {
        let _ = Conn::open(target).and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    }
    let _ = std::fs::remove_dir_all(root);
}

fn world(target: &str, tag: &str) -> (PathBuf, String) {
    let root = project_copy(tag);
    let url = fresh(target, tag);
    let dir = std::env::temp_dir().join(format!("quadra-apps-{}-{tag}-gen", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra")).args(["bench", "gen", "--scale", "tiny", "--out"]).arg(&dir).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let checked = quadra_analysis::check(&quadra_sema::Project::load(&root).unwrap());
    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error).map(|d| d.message.clone()).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    quadra_store::ops::init(&url, m, r, clock()).unwrap();
    quadra_store::bulk::import_dir(&url, m, r, &dir, clock(), None, &mut |_| {}).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (root, url)
}

fn pick(url: &str, sql: &str) -> u64 {
    Conn::open(url).unwrap().query(sql, &[]).unwrap().into_iter().next().and_then(|r| r.into_iter().next().flatten()).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("no row: {sql}"))
}

struct T {
    local: Local,
}

impl T {
    fn call(&mut self, user: u64, method: &str, path: &str, body: J) -> (u16, J) {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let q: Vec<(&str, &str)> = query.split('&').filter(|s| !s.is_empty()).filter_map(|p| p.split_once('=')).collect();
        self.local.views(user, method, path, &q, &body)
    }

    fn ok(&mut self, user: u64, method: &str, path: &str, body: J) -> J {
        let (s, b) = self.call(user, method, path, body);
        assert!(s == 200 || s == 201, "{method} {path} as #{user}: {s} {b}");
        b
    }

    fn apps(&mut self, user: u64) -> Vec<u64> {
        let b = self.ok(user, "GET", "/v1/views?kind=app", J::Null);
        b["views"].as_array().unwrap().iter().map(|v| v["id"].as_u64().unwrap()).collect()
    }

    /// A workbook's formulas as `user`: the cells of the answer by `sheet!A1`.
    fn eval(&mut self, user: u64, sheets: J) -> J {
        let (s, b) = self.local.eval(user, &json!({ "sheets": sheets }));
        assert_eq!(s, 200, "{b}");
        b["cells"].clone()
    }

    /// One formula on its own, as `user`.
    fn one(&mut self, user: u64, f: &str) -> J {
        let c = self.eval(user, json!([{ "id": "s1", "name": "Sheet1", "cells": { "A1": { "v": f } } }]));
        c["s1!A1"].clone()
    }

    fn total(&mut self, user: u64, q: J) -> J {
        let (s, b) = self.local.query(user, &q);
        assert_eq!(s, 200, "{q}: {b}");
        b
    }

    fn tx(&mut self, user: u64, ops: J, key: &str) -> (u16, J) {
        let base = self.local.query(user, &json!({ "entity": "Region", "fields": [], "limit": 1 })).1["base"].as_u64().unwrap();
        self.local.tx(user, &json!({ "base": base, "ops": ops }), key)
    }
}

fn sheet(cells: J) -> J {
    json!({ "id": "s1", "name": "Sheet1", "mode": "free", "rows": 100, "cols": 10, "cells": cells })
}

fn app_config(sheets: J) -> J {
    json!({ "version": 1, "pages": [{ "id": "p1", "name": "Page 1", "components": [
        { "id": "c1", "type": "sheet", "title": "Sales", "layout": { "x": 0, "y": 0, "w": 12, "h": 8 }, "config": { "sheets": sheets }, "bindings": {} },
        { "id": "c2", "type": "markdown", "layout": { "x": 12, "y": 0, "w": 12, "h": 4 }, "config": { "text": "# Hello" } },
        { "id": "c3", "type": "gauge", "layout": { "x": 0, "y": 8, "w": 6, "h": 4 }, "config": { "kind": "bar", "anything": [1, 2] } }
    ] }] })
}

fn d(c: &J) -> String {
    c["d"].as_str().unwrap_or("").to_string()
}

fn apps_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| pick(&url, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"));
    let (admin, s1, s2, manager, controller) = (user("admin"), user("sales0001"), user("sales0002"), user("manager01"), user("controller"));
    let mut t = T { local: Local::open(&root, &url, 64).unwrap() };

    // ---- apps: stored and shared like views ----
    let cfg = app_config(json!([sheet(json!({ "A1": { "v": "Orders", "s": { "b": true } }, "B1": { "v": "=Order.count()" } }))]));
    let created = t.ok(s1, "POST", "/v1/views", json!({ "kind": "app", "type": "app", "name": "Desk", "config": cfg }));
    let app = created["view"]["id"].as_u64().unwrap();
    assert_eq!(created["view"]["kind"], json!("app"));
    assert_eq!(created["view"]["entity"], J::Null);
    // an unknown component type is kept as it is
    assert_eq!(created["view"]["config"]["pages"][0]["components"][2]["config"]["anything"], json!([1, 2]));
    assert!(t.apps(s1).contains(&app));
    assert!(!t.apps(s2).contains(&app), "private: only its owner");
    assert_eq!(t.call(s2, "GET", &format!("/v1/views/{app}"), J::Null).0, 404);
    // apps are not views of an entity
    let views = t.ok(s1, "GET", "/v1/views?entity=Order", J::Null);
    assert!(!views["views"].as_array().unwrap().iter().any(|v| v["id"] == json!(app)));
    // a configuration that does not fit
    let (st, b) = t.call(s1, "POST", "/v1/views", json!({ "kind": "app", "name": "Bad", "config": { "pages": [{ "id": "p1", "components": [{ "id": "x", "type": "sheet", "layout": { "x": 20, "y": 0, "w": 10, "h": 2 }, "config": { "sheets": [] } }] }] } }));
    assert_eq!(st, 400, "{b}");
    assert!(b["diagnostics"].to_string().contains("layout"), "{b}");
    // shared with a team, to use
    let team = t.ok(admin, "POST", "/v1/teams", json!({ "name": "North desk", "members": [{ "id": s2, "owner": false }] }))["team"]["id"].as_u64().unwrap();
    let v1 = created["view"]["version"].as_u64().unwrap();
    let shared = t.ok(s1, "PUT", &format!("/v1/views/{app}/shares"), json!({ "base": v1, "shares": [{ "type": "team", "id": team, "edit": false }] }));
    let v2 = shared["view"]["version"].as_u64().unwrap();
    assert!(t.apps(s2).contains(&app));
    let seen = t.ok(s2, "GET", &format!("/v1/views/{app}"), J::Null);
    assert_eq!(seen["view"]["access"], json!("use"));
    let (st, _) = t.call(s2, "PUT", &format!("/v1/views/{app}"), json!({ "base": v2, "name": "Mine" }));
    assert_eq!(st, 403, "use only");
    // co-owner: changes it; a stale base is refused with what changed
    let v3 = t.ok(s1, "PUT", &format!("/v1/views/{app}/shares"), json!({ "base": v2, "shares": [{ "type": "team", "id": team, "edit": true }] }))["view"]["version"].as_u64().unwrap();
    let v4 = t.ok(s2, "PUT", &format!("/v1/views/{app}"), json!({ "base": v3, "name": "Desk (north)" }))["view"]["version"].as_u64().unwrap();
    let (st, b) = t.call(s1, "PUT", &format!("/v1/views/{app}"), json!({ "base": v3, "config": cfg }));
    assert_eq!(st, 409, "{b}");
    assert!(b["conflicts"].to_string().contains("name"), "{b}");
    // the history
    let h = t.ok(s1, "GET", &format!("/v1/views/{app}/history"), J::Null);
    assert!(h["entries"].as_array().unwrap().len() >= 4);
    // not shared any more: gone from the member's list
    t.ok(admin, "PUT", &format!("/v1/teams/{team}"), json!({ "base": 1, "members": [] }));
    assert!(!t.apps(s2).contains(&app));
    let _ = v4;

    // ---- formulas: references, ranges, other sheets ----
    let book = json!([
        { "id": "s1", "name": "Sheet1", "cells": {
            "A1": { "v": "10" }, "A2": { "v": "2.5" }, "A3": { "v": "=A1 * A2" }, "A4": { "v": "=A1:A3.sum()" },
            "B1": { "v": "5.40 EUR" }, "B2": { "v": "=B1 * 3" }, "B3": { "v": "=B1 + 3 pcs" },
            "C1": { "v": "=Data!A1 + 'My data'!B2" }, "C2": { "v": "=A1:A3.count()" }, "C3": { "v": "=A1:A2.avg()" },
            "D1": { "v": "=D2 + 1" }, "D2": { "v": "=D1 + 1" }, "D3": { "v": "=D1 * 2" },
            "E1": { "v": "=Z99 ?? 7" }, "E2": { "v": "=Z99 + 1" }, "E3": { "v": "=Nope!A1" }, "E4": { "v": "=#REF! + 1" },
            "F1": { "v": "=[1, 2, 3].map(x => x * 10)" }, "F3": { "v": "blocks" }, "G1": { "v": "=[(1, \"a\"), (2, \"b\")]" },
            "J1": { "v": "=G1#.count() + F1#.sum()" }, "J2": { "v": "=G1#.count()" }, "H3": { "v": "=today() >= 2020-01-01" }, "K1": { "v": "=K1" },
            "I1": { "v": "'00123" }, "I2": { "v": "=I1.len()" }, "I3": { "v": "=(A1 + 1" }, "I4": { "v": "=G2" },
        } },
        { "id": "s2", "name": "Data", "cells": { "A1": { "v": "100" } } },
        { "id": "s3", "name": "My data", "cells": { "B2": { "v": "=Data!A1 / 4" } } },
    ]);
    let c = t.eval(s1, book.clone());
    assert_eq!(d(&c["s1!A3"]), "25", "{c}");
    assert_eq!(d(&c["s1!A4"]), "37.5");
    assert_eq!(d(&c["s1!B2"]), "16.20 EUR");
    assert_eq!(c["s1!B3"]["code"], json!("#TYPE"), "{}", c["s1!B3"]);
    assert!(c["s1!B3"]["error"].as_str().unwrap().contains("cannot"), "{}", c["s1!B3"]);
    assert_eq!(d(&c["s1!C1"]), "125");
    assert_eq!(c["s1!C2"]["v"], json!(3));
    assert_eq!(d(&c["s1!C3"]), "6.25");
    for k in ["s1!D1", "s1!D2", "s1!D3", "s1!K1"] {
        assert_eq!(c[k]["code"], json!("#CYCLE"), "{k}: {}", c[k]);
    }
    assert!(c["s1!D1"]["error"].as_str().unwrap().contains("D1 → D2 → D1") || c["s1!D1"]["error"].as_str().unwrap().contains("D2 → D1"), "{}", c["s1!D1"]);
    assert_eq!(c["s1!E1"]["v"], json!(7), "an empty cell is none");
    assert_eq!(c["s1!E2"]["code"], json!("#ERROR"), "none + 1: {}", c["s1!E2"]);
    assert_eq!(c["s1!E3"]["code"], json!("#REF"));
    assert_eq!(c["s1!E4"]["code"], json!("#REF"));
    // F1 spills down into F3, which holds something
    assert_eq!(c["s1!F1"]["code"], json!("#SPILL"), "{}", c["s1!F1"]);
    // a list of tuples spills down and right; its cells can be read
    assert_eq!(c["s1!G1"]["spill"]["rows"], json!(2));
    assert_eq!(c["s1!G1"]["spill"]["cols"], json!(2));
    assert_eq!(c["s1!G1"]["spill"]["cells"][1][1]["v"], json!("b"));
    assert_eq!(c["s1!I4"]["v"], json!(2), "a cell of a spill: {}", c["s1!I4"]);
    assert_eq!(c["s1!J1"]["code"], json!("#SPILL"), "F1# is in error: {}", c["s1!J1"]);
    assert_eq!(c["s1!J2"]["v"], json!(2), "{}", c["s1!J2"]);
    assert_eq!(c["s1!H3"]["v"], json!(true));
    assert_eq!(c["s1!I2"]["v"], json!(5), "'00123 is text");
    assert_eq!(c["s1!I3"]["code"], json!("#ERROR"));
    // only what changed: A1 and what uses it
    let (st, b) = t.local.eval(s1, &json!({ "sheets": book, "changed": ["s1!A1"] }));
    assert_eq!(st, 200);
    assert_eq!(b["partial"], json!(true));
    let keys: Vec<&String> = b["cells"].as_object().unwrap().keys().collect();
    assert!(keys.iter().any(|k| *k == "s1!A3") && keys.iter().any(|k| *k == "s1!A4") && !keys.iter().any(|k| *k == "s1!B2"), "{keys:?}");
    // a spill freed: F1 fills F1:F3
    let c = t.eval(s1, json!([sheet(json!({ "F1": { "v": "=[1, 2, 3].map(x => x * 10)" }, "H1": { "v": "=F1#.sum() + F2" } }))]));
    assert_eq!(c["s1!F1"]["spill"]["rows"], json!(3));
    assert_eq!(c["s1!H1"]["v"], json!(80), "{}", c["s1!H1"]);
    // formulas that would write, or act as someone else, are refused
    for f in ["=Region(code=\"X\", name=\"x\")", "=as admin: Order.count()", "=login(\"admin\")"] {
        let r = t.one(s1, f);
        assert!(r["error"].is_string(), "{f}: {r}");
    }

    // ---- formulas over the database: what each user may read ----
    for (who, login) in [(admin, "admin"), (s1, "sales0001"), (manager, "manager01"), (controller, "controller")] {
        let q = t.total(who, json!({ "entity": "Order", "fields": [], "limit": 1 }));
        let n = t.one(who, "=Order.count()");
        assert_eq!(n["v"], q["total"], "{login}: Order.count() as a query gives");
        assert_eq!(n["db"], json!(true));
        let agg = t.total(who, json!({ "entity": "Order", "fields": [], "where": "state is Confirmed", "aggregates": { "total": "sum" }, "limit": 1 }));
        let s = t.one(who, "=Order.where(o => o.state is Confirmed).sum(o => o.total)");
        assert_eq!(s["v"]["money"].as_str().map(|x| x.trim_end_matches('0').trim_end_matches('.').to_string()), agg["aggregates"]["total"]["v"]["money"].as_str().map(|x| x.trim_end_matches('0').trim_end_matches('.').to_string()), "{login}: {s} vs {agg}");
        let groups = t.total(who, json!({ "entity": "Order", "group_by": ["customer.region"], "limit": 1000 }));
        let g = t.one(who, "=Order.group_by(o => o.customer.region).map(g => (g.key.name, g.items.count()))");
        let n_groups = groups["groups"].as_array().unwrap().len();
        if n_groups > 1 {
            assert_eq!(g["spill"]["rows"], json!(n_groups), "{login}: {g}");
            let a: u64 = g["spill"]["cells"].as_array().unwrap().iter().map(|r| r[1]["v"].as_u64().unwrap()).sum();
            assert_eq!(json!(a), q["total"], "{login}: the groups hold every order they may read");
        }
        let m = t.one(who, "=confirmed_orders()");
        assert_eq!(m["certified"], json!("confirmed_orders"), "{m}");
    }
    // a hidden field: refused, in the expression and in a query's lambdas
    let h1 = t.one(s1, "=Customer.sum(c => c.credit_limit)");
    assert_eq!(h1["code"], json!("#HIDDEN"), "{h1}");
    let h2 = t.one(s1, "=Customer.where(c => c.credit_limit > 0 EUR).count()");
    assert_eq!(h2["code"], json!("#HIDDEN"), "{h2}");
    let h3 = t.one(s1, "=Customer.first()?.credit_limit");
    assert_eq!(h3["code"], json!("#HIDDEN"), "{h3}");
    let ok = t.one(manager, "=Customer.sum(c => c.credit_limit)");
    assert!(ok["error"].is_null(), "a manager sees it again: {ok}");
    // a record the user may not read
    let other = pick(&url, &format!("SELECT CAST(c.\"id\" AS TEXT) FROM \"customer\" c JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" <> {s1} AND (r.\"manager_id\" IS NULL OR r.\"manager_id\" <> {s1}) ORDER BY c.\"id\" LIMIT 1"));
    let readable = t.total(s1, json!({ "entity": "Customer", "fields": [], "where": format!("id == {other}"), "limit": 1 }))["total"].as_u64().unwrap();
    if readable == 0 {
        let r = t.one(s1, &format!("=Customer.find(c => c.id == {other})"));
        assert_eq!(r["v"], J::Null, "not found when not readable: {r}");
    }

    // ---- kept until a commit changes what it reads ----
    let before = t.one(admin, "=Region.count()");
    let regions = t.one(admin, "=Region.where(r => r.active).count()");
    let (st, b) = t.tx(admin, json!([{ "op": "create", "entity": "Region", "values": { "code": "ZZ", "name": "Zeta", "manager": admin } }]), "apps-region-1");
    assert_eq!(st, 200, "{b}");
    let after = t.one(admin, "=Region.count()");
    assert_eq!(after["v"].as_u64().unwrap(), before["v"].as_u64().unwrap() + 1, "a commit of a Region: computed again");
    let again = t.one(admin, "=Region.where(r => r.active).count()");
    assert_eq!(again["v"].as_u64().unwrap(), regions["v"].as_u64().unwrap() + 1);

    // ---- a time limit, with a clear error ----
    let (st, b) = t.local.eval(admin, &json!({ "timeout_ms": 1, "sheets": [sheet(json!({ "A1": { "v": "=[1..2000].count() + (1..200000).map(x => x * 2).count()" } }))] }));
    assert_eq!(st, 200);
    assert_eq!(b["cells"]["s1!A1"]["code"], json!("#TIMEOUT"), "{}", b["cells"]["s1!A1"]);

    // ---- the Excel file: values, types, styles, sheet names, merges, formulas in notes ----
    let book = json!([
        { "id": "s1", "name": "Summary", "cols_w": { "A": 160 }, "rows_h": { "1": 30 }, "merges": ["A5:B5"], "freeze": { "rows": 1, "cols": 0 }, "cells": {
            "A1": { "v": "Orders", "s": { "b": true, "bg": "#FFEE00", "fg": "#112233", "bd": { "b": "thin" } } },
            "B1": { "v": "=Order.count()" },
            "A2": { "v": "5.40 EUR" }, "B2": { "v": "=A2 * 2", "s": { "i": true } },
            "A3": { "v": "2026-11-21" }, "B3": { "v": "=Region.where(r => r.active).map(r => (r.code, r.customer_count))" },
            "A4": { "v": "25%" }, "A5": { "v": "merged" },
        } },
        { "id": "s2", "name": "Raw/data?", "cells": { "A1": { "v": "x" } } },
    ]);
    let cfg = app_config(book);
    let created = t.ok(admin, "POST", "/v1/views", json!({ "kind": "app", "name": "Export", "config": cfg }));
    let exp = created["view"]["id"].as_u64().unwrap();
    let (st, b) = t.call(admin, "GET", &format!("/v1/apps/{exp}/export.xlsx?component=c1"), J::Null);
    assert_eq!(st, 200, "{b}");
    assert!(b["__file"]["name"].as_str().unwrap().ends_with(".xlsx"));
    let bytes = quadra_server::unhex(b["__file"]["hex"].as_str().unwrap()).unwrap();
    let orders = t.total(admin, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].as_u64().unwrap();
    {
        use calamine::{Data, Reader, Xlsx};
        let mut x: Xlsx<_> = Xlsx::new(std::io::Cursor::new(bytes.clone())).unwrap();
        assert_eq!(x.sheet_names(), vec!["Summary".to_string(), "Raw_data_".to_string()]);
        let r = x.worksheet_range("Summary").unwrap();
        assert_eq!(r.get_value((0, 0)), Some(&Data::String("Orders".into())));
        assert_eq!(r.get_value((0, 1)), Some(&Data::Float(orders as f64)), "a count as a number");
        assert_eq!(r.get_value((1, 0)), Some(&Data::Float(5.4)), "an amount as a number");
        assert_eq!(r.get_value((1, 1)), Some(&Data::Float(10.8)));
        assert!(matches!(r.get_value((2, 0)), Some(Data::DateTime(_))), "a date as a date: {:?}", r.get_value((2, 0)));
        assert_eq!(r.get_value((3, 0)), Some(&Data::Float(0.25)), "a percent as a fraction");
        // the spill fills its cells
        assert!(matches!(r.get_value((2, 1)), Some(Data::String(_))), "{:?}", r.get_value((2, 1)));
        assert!(matches!(r.get_value((2, 2)), Some(Data::Float(_))), "{:?}", r.get_value((2, 2)));
        x.load_merged_regions().unwrap();
        assert_eq!(x.merged_regions_by_sheet("Summary").len(), 1);
    }
    {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut read = |name: &str| -> String {
            use std::io::Read;
            let mut s = String::new();
            z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
            s
        };
        let styles = read("xl/styles.xml");
        assert!(styles.contains("<b/>") && styles.contains("<i/>"), "bold and italic");
        assert!(styles.contains("FFFFEE00") && styles.contains("FF112233"), "fill and font colors");
        assert!(styles.contains("&quot;EUR&quot;"), "the currency in the number format: {styles}");
        assert!(styles.contains("thin"), "a border");
        let sheet1 = read("xl/worksheets/sheet1.xml");
        assert!(sheet1.contains("<pane") && sheet1.contains("ySplit=\"1\""), "frozen panes");
        assert!(sheet1.contains("customWidth") && sheet1.contains("customHeight"), "widths and heights");
        let notes = read("xl/comments1.xml");
        assert!(notes.contains("Quadra: =Order.count()"), "the formula in a note: {notes}");
    }
    // as the salesperson: their orders only
    t.ok(admin, "PUT", &format!("/v1/views/{exp}/shares"), json!({ "base": created["view"]["version"], "shares": [{ "type": "everyone", "edit": false }] }));
    let (st, b) = t.call(s1, "GET", &format!("/v1/apps/{exp}/export.xlsx?component=c1"), J::Null);
    assert_eq!(st, 200);
    let bytes = quadra_server::unhex(b["__file"]["hex"].as_str().unwrap()).unwrap();
    {
        use calamine::{Data, Reader, Xlsx};
        let mut x: Xlsx<_> = Xlsx::new(std::io::Cursor::new(bytes)).unwrap();
        let r = x.worksheet_range("Summary").unwrap();
        let mine = t.total(s1, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].as_u64().unwrap();
        assert!(mine < orders);
        assert_eq!(r.get_value((0, 1)), Some(&Data::Float(mine as f64)), "the export runs as whoever asks");
    }
    let (st, _) = t.call(s2, "GET", "/v1/apps/999999/export.xlsx?component=c1", J::Null);
    assert_eq!(st, 404);

    // exported and imported with the views (`quadra db views export|import`)
    let file = std::env::temp_dir().join(format!("quadra-apps-{}-{tag}.json", std::process::id()));
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_quadra")).args(args).arg(&root).args(["--url", &url]).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["db", "views", "export", file.to_str().unwrap()]);
    let exported: J = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let app_out = exported["views"].as_array().unwrap().iter().find(|v| v["name"] == json!("Export")).cloned().unwrap();
    assert_eq!(app_out["kind"], json!("app"));
    assert_eq!(app_out["entity"], J::Null);
    assert_eq!(app_out["config"]["pages"][0]["components"][0]["config"]["sheets"][0]["name"], json!("Summary"));
    let before = t.apps(admin).len();
    drop(t);
    run(&["db", "views", "import", file.to_str().unwrap()]);
    let mut t = T { local: Local::open(&root, &url, 64).unwrap() };
    assert!(t.apps(admin).len() > before, "the apps imported again");
    let _ = std::fs::remove_file(&file);

    drop(t);
    cleanup(target, &url, &root);
}

#[test]
fn apps_sqlite() {
    apps_on("sqlite");
}

#[test]
fn apps_postgres() {
    match std::env::var("QUADRA_TEST_PG") {
        Ok(url) if !url.is_empty() => apps_on(&url),
        _ => eprintln!("QUADRA_TEST_PG not set: skipped"),
    }
}
