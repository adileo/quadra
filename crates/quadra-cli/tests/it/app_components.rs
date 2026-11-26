//! The components of apps that act on the data (engine/params.rs, engine/apps.rs, the
//! `filters` of `POST /v1/query`): global filters as typed parameters of formulas (`@region`,
//! checked once, evaluated again when a value changes), filters bound to entities through
//! reference paths (`Order.…`, `OrderLine.order.customer.region`), the same filters on the
//! queries of embedded views and boards, what each user sees through them (row rules, hidden
//! fields), actions and flow transitions run by buttons (with the reasons they are refused),
//! forms creating and editing records (validation, 409), the checks of every component's
//! configuration, per-user default filters, and the Excel export of a chart, a KPI and a
//! spreadsheet with filters. On `examples/stress` (the `tiny` world), on SQLite and, when
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
    let dir = std::env::temp_dir().join(format!("quadra-appc-{}-{tag}", std::process::id()));
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
        let p = std::env::temp_dir().join(format!("quadra-appc-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qac_{}_{tag}", std::process::id());
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
    let dir = std::env::temp_dir().join(format!("quadra-appc-{}-{tag}-gen", std::process::id()));
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

fn text(url: &str, sql: &str) -> String {
    Conn::open(url).unwrap().query(sql, &[]).unwrap().into_iter().next().and_then(|r| r.into_iter().next().flatten()).unwrap_or_else(|| panic!("no row: {sql}"))
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

    /// `POST /v1/eval` with expressions and parameters: the answer.
    fn eval(&mut self, user: u64, body: J) -> J {
        let (s, b) = self.local.eval(user, &body);
        assert_eq!(s, 200, "{body}: {b}");
        b
    }

    fn query(&mut self, user: u64, q: J) -> J {
        let (s, b) = self.local.query(user, &q);
        assert_eq!(s, 200, "{q}: {b}");
        b
    }

    fn base(&mut self, user: u64) -> u64 {
        self.local.query(user, &json!({ "entity": "Region", "fields": [], "limit": 1 })).1["base"].as_u64().unwrap()
    }

    fn tx(&mut self, user: u64, ops: J, key: &str) -> (u16, J) {
        let base = self.base(user);
        self.local.tx(user, &json!({ "base": base, "ops": ops }), key)
    }
}

fn money(c: &J) -> f64 {
    c["v"]["money"].as_str().map(|x| x.parse().unwrap()).or_else(|| c["v"].as_f64()).unwrap_or_else(|| panic!("not an amount: {c}"))
}

/// The filters of the Sales cockpit: region (several), period, state.
fn filters(regions: J, period: J, states: J) -> J {
    json!([
        { "name": "region", "entity": "Order", "field": "customer.region", "widget": "multi", "value": regions },
        { "name": "period", "entity": "Order", "field": "date", "widget": "date_range", "value": period },
        { "name": "state", "entity": "Order", "field": "state", "widget": "multi", "value": states },
    ])
}

fn components_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| pick(&url, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"));
    let (admin, s1) = (user("admin"), user("sales0001"));
    let mut t = T { local: Local::open(&root, &url, 64).unwrap() };
    let r01 = pick(&url, "SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = 'R01'");
    let r02 = pick(&url, "SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = 'R02'");

    // ---- filters as parameters of formulas ----
    let exprs = json!({
        "n": "Order.count(o => (@region.count() == 0 or o.customer.region in @region) and o.date in @period)",
        "auto": "Order.count()",
        "lines": "OrderLine.count()",
        "sum": "Order.sum(o => o.total)",
        "by_state": "Order.group_by(o => o.state).map(g => (state: g.key, n: g.items.count()))",
    });
    let bind = json!({ "Order": { "region": "customer.region", "period": "date", "state": "state" }, "OrderLine": { "region": "order.customer.region", "period": "order.date" } });
    let ask = |t: &mut T, who: u64, f: J| t.eval(who, json!({ "sheets": [], "exprs": exprs, "params": f, "bind": bind }));
    let all = ask(&mut t, admin, filters(json!([]), J::Null, json!([])));
    let total = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].as_u64().unwrap();
    assert_eq!(all["exprs"]["n"]["v"], json!(total), "no filter set: every order");
    assert_eq!(all["exprs"]["auto"]["v"], json!(total));
    let lines_all = all["exprs"]["lines"]["v"].as_u64().unwrap_or_else(|| panic!("{all}"));
    // one region: the formula's own condition and the bound one agree with a query
    let one = ask(&mut t, admin, filters(json!([r01]), J::Null, json!([])));
    let q01 = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "where": format!("customer.region.code == \"R01\"") }))["total"].as_u64().unwrap();
    assert!(q01 > 0 && q01 < total);
    assert_eq!(one["exprs"]["n"]["v"], json!(q01), "{one}");
    assert_eq!(one["exprs"]["auto"]["v"], json!(q01), "bound to Order through customer.region");
    let l01 = one["exprs"]["lines"]["v"].as_u64().unwrap();
    assert!(l01 > 0 && l01 < lines_all, "bound to OrderLine through order.customer.region: {l01} of {lines_all}");
    // the groups of the bound entity: in SQL, filtered
    let groups: u64 = one["exprs"]["by_state"]["spill"]["cells"].as_array().unwrap_or_else(|| panic!("{}", one["exprs"]["by_state"])).iter().map(|r| r[1]["v"].as_u64().unwrap_or_else(|| panic!("{}", one["exprs"]["by_state"]))).sum();
    assert_eq!(groups, q01);
    assert_eq!(one["exprs"]["by_state"]["spill"]["header"], json!(["state", "n"]), "named columns");
    // a period, and a state
    let p = ask(&mut t, admin, filters(json!([r01, r02]), json!(["2026-01-01", "2026-06-30"]), json!(["Confirmed"])));
    let qp = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "where": "customer.region.code in {\"R01\", \"R02\"} and date >= 2026-01-01 and date <= 2026-06-30 and state is Confirmed" }))["total"].as_u64().unwrap();
    assert_eq!(p["exprs"]["auto"]["v"], json!(qp), "{p}");

    // the check is made once: a new value is evaluated with the checked workbook kept
    t.local.clear_sheets();
    let _ = ask(&mut t, admin, filters(json!([r01]), J::Null, json!([])));
    let checks = t.local.sheet_checks();
    let again = ask(&mut t, admin, filters(json!([r02]), json!(["2026-01-01", null]), json!(["Draft"])));
    assert_eq!(t.local.sheet_checks(), checks, "a filter's value changed: nothing checked again");
    assert_ne!(again["exprs"]["auto"]["v"], one["exprs"]["auto"]["v"]);
    // the values kept are per value of the filters
    let back = ask(&mut t, admin, filters(json!([r01]), J::Null, json!([])));
    assert_eq!(back["exprs"]["auto"]["v"], json!(q01));

    // type errors: `@region` is a list of regions
    let bad = t.eval(admin, json!({ "sheets": [], "exprs": { "x": "@region.first()?.nope", "w": "@amount_min + 3 pcs", "v": "@amount_min + 3.00 EUR", "y": "@nope", "z": "@period.contains(2026-01-01)" }, "params": [filters(json!([]), J::Null, json!([]))[0].clone(), filters(json!([]), J::Null, json!([]))[1].clone(), { "name": "amount", "entity": "Order", "field": "total", "widget": "number_range", "value": null }] }));
    assert_eq!(bad["exprs"]["x"]["code"], json!("#TYPE"), "{bad}");
    assert_eq!(bad["exprs"]["w"]["code"], json!("#TYPE"), "an amount of the orders' total is in EUR: {bad}");
    assert_eq!(bad["exprs"]["v"]["v"], J::Null, "no amount chosen: none: {bad}");
    assert_eq!(bad["exprs"]["y"]["code"], json!("#REF"), "{bad}");
    assert!(bad["exprs"]["y"]["error"].as_str().unwrap().contains("@region"), "{bad}");
    assert_eq!(bad["exprs"]["z"]["v"], json!(true), "{bad}");
    // a parameter that does not fit
    let (st, b) = t.local.eval(admin, &json!({ "exprs": { "x": "1" }, "params": [{ "name": "r", "entity": "Order", "field": "customer.nope", "widget": "select" }] }));
    assert_eq!(st, 400, "{b}");
    let (st, b) = t.local.eval(admin, &json!({ "exprs": { "x": "1" }, "params": [{ "name": "d", "entity": "Order", "field": "state", "widget": "date_range" }] }));
    assert_eq!(st, 400, "{b}");
    let (st, b) = t.local.eval(admin, &json!({ "exprs": { "x": "1" }, "params": filters(json!([]), J::Null, json!([])), "bind": { "Order": { "region": "number" } } }));
    assert_eq!(st, 400, "a region filter bound to a text: {b}");

    // ---- the salesperson: their rows only, through every filter ----
    let mine = ask(&mut t, s1, filters(json!([]), J::Null, json!([])));
    let q_s1 = t.query(s1, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].as_u64().unwrap();
    assert!(q_s1 < total);
    assert_eq!(mine["exprs"]["auto"]["v"], json!(q_s1), "{mine}");
    // the lines of their orders (the lines of a document are read with it)
    let (st, ql) = t.local.query(s1, &json!({ "entity": "OrderLine", "fields": [], "limit": 1 }));
    if st == 200 {
        assert_eq!(mine["exprs"]["lines"]["v"], ql["total"], "{mine}");
    } else {
        assert_eq!(mine["exprs"]["lines"]["code"], json!("#NOACCESS"), "{mine}");
    }

    // ---- the same filters on queries (embedded views, boards) ----
    let qf = |regions: J| json!([{ "path": "customer.region", "op": "in", "value": regions }, { "path": "date", "op": "range", "value": ["2026-01-01", "2026-06-30"] }, { "path": "state", "op": "in", "value": ["Confirmed"] }]);
    let rows = t.query(admin, json!({ "entity": "Order", "fields": ["number"], "limit": 5, "filters": qf(json!([r01, r02])) }));
    assert_eq!(rows["total"], json!(qp), "{rows}");
    // through references, on another entity
    let lines = t.query(admin, json!({ "entity": "OrderLine", "fields": [], "limit": 1, "filters": [{ "path": "order.customer.region", "op": "in", "value": [r01] }] }));
    assert_eq!(lines["total"], json!(l01));
    // groups (a board's columns) and aggregates
    let g = t.query(admin, json!({ "entity": "Order", "group_by": ["state"], "limit": 10, "filters": [{ "path": "customer.region", "op": "eq", "value": r01 }] }));
    assert_eq!(g["sql"], json!(true), "{g}");
    assert_eq!(g["groups"].as_array().unwrap().iter().map(|x| x["count"].as_u64().unwrap()).sum::<u64>(), q01);
    let a = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "aggregates": { "total": "sum" }, "filters": [{ "path": "customer.region", "op": "eq", "value": r01 }] }));
    assert!((money(&a["aggregates"]["total"]) - money(&one["exprs"]["sum"])).abs() < 0.01, "{a} vs {one}");
    // numbers, text, yes/no
    let big = t.query(admin, json!({ "entity": "Customer", "fields": [], "limit": 1, "filters": [{ "path": "credit_limit", "op": "range", "value": [60000, null] }] }))["total"].as_u64().unwrap();
    let big_w = t.query(admin, json!({ "entity": "Customer", "fields": [], "limit": 1, "where": "credit_limit >= 60000 EUR" }))["total"].as_u64().unwrap();
    assert_eq!(big, big_w);
    let name = text(&url, "SELECT \"name\" FROM \"customer\" ORDER BY \"id\" LIMIT 1");
    let word: String = name.split_whitespace().next().unwrap().to_string();
    let c = t.query(admin, json!({ "entity": "Customer", "fields": [], "limit": 1, "filters": [{ "path": "name", "op": "contains", "value": word }, { "path": "active", "op": "true" }] }))["total"].as_u64().unwrap();
    let c_w = t.query(admin, json!({ "entity": "Customer", "fields": [], "limit": 1, "where": format!("name.contains(\"{word}\") and active") }))["total"].as_u64().unwrap();
    assert_eq!(c, c_w);
    // the salesperson: their rows; a hidden field cannot filter
    let theirs = t.query(s1, json!({ "entity": "Order", "fields": [], "limit": 1, "filters": [{ "path": "state", "op": "in", "value": ["Draft", "Confirmed", "Shipped", "Cancelled"] }] }))["total"].as_u64().unwrap();
    assert_eq!(theirs, q_s1);
    let (st, b) = t.local.query(s1, &json!({ "entity": "Customer", "fields": [], "limit": 1, "filters": [{ "path": "credit_limit", "op": "range", "value": [1, null] }] }));
    assert_eq!(st, 403, "{b}");
    let (st, _) = t.local.query(admin, &json!({ "entity": "Order", "fields": [], "limit": 1, "filters": [{ "path": "state", "op": "contains", "value": "x" }] }));
    assert_eq!(st, 400);

    // ---- the Sales cockpit (examples/stress/apps), imported as `quadra db views import` does ----
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress/apps/sales-cockpit.json");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_quadra")).args(["db", "views", "import"]).arg(&file).arg(&root).args(["--url", &url]).output().unwrap();
    assert!(st.status.success(), "{}{}", String::from_utf8_lossy(&st.stdout), String::from_utf8_lossy(&st.stderr));
    let mut t = T { local: Local::open(&root, &url, 64).unwrap() };
    let list = t.ok(s1, "GET", "/v1/views?kind=app", J::Null);
    let app = list["views"].as_array().unwrap().iter().find(|v| v["name"] == json!("Sales cockpit")).expect("shared with everyone").clone();
    let app_id = app["id"].as_u64().unwrap();
    let cfg = app["config"].clone();
    assert!(app["broken"].is_null(), "{app}");
    let kpi = |r: &J, c: &str| r["results"][c][0]["exprs"]["value"].clone();
    for (who, login) in [(admin, "admin"), (s1, "sales0001")] {
        let r = t.local.open_app(who, &cfg, &json!({}));
        assert!(r["errors"].as_array().unwrap().is_empty(), "{login}: {}", r["errors"]);
        // every figure is what the user's queries give
        let orders = t.query(who, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].clone();
        assert_eq!(kpi(&r, "k_orders")["v"], orders, "{login}");
        let board: u64 = r["results"]["v_board"][0]["groups"].as_array().unwrap().iter().map(|g| g["count"].as_u64().unwrap()).sum();
        assert_eq!(json!(board), orders, "{login}: the board's columns");
        let top = r["results"]["c_top"][0]["exprs"]["data"]["spill"]["rows"].as_u64().unwrap();
        assert!(top > 0 && top <= 10, "{login}: {top}");
        assert_eq!(r["results"]["c_monthly"][0]["exprs"]["data"]["spill"]["header"], json!(["month", "revenue"]));
        assert_eq!(r["results"]["s_pivot"][0]["cells"]["s1!G1"]["v"], orders, "{login}: the sheet bound to the filters");
    }
    // with filters: every component follows them
    let f = json!({ "region": [r01], "period": ["2026-01-01", "2026-12-31"] });
    let r = t.local.open_app(admin, &cfg, &f);
    assert!(r["errors"].as_array().unwrap().is_empty(), "{}", r["errors"]);
    let q = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "where": "customer.region.code == \"R01\" and date >= 2026-01-01 and date <= 2026-12-31" }))["total"].clone();
    assert_eq!(kpi(&r, "k_orders")["v"], q);
    assert_eq!(r["results"]["s_pivot"][0]["cells"]["s1!G1"]["v"], q);
    assert_eq!(r["results"]["s_pivot"][0]["cells"]["s1!G2"]["v"], json!(1), "@region in a cell");
    let board: u64 = r["results"]["v_board"][0]["groups"].as_array().unwrap().iter().map(|g| g["count"].as_u64().unwrap()).sum();
    assert_eq!(json!(board), q, "the board follows the filters");
    // the previous period is asked too (a second evaluation of the KPI with the range moved back)
    assert_eq!(r["results"]["k_orders"].as_array().unwrap().len(), 2, "{}", r["results"]["k_orders"]);
    let prev = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "where": "customer.region.code == \"R01\" and date >= 2025-01-01 and date <= 2025-12-31" }))["total"].clone();
    assert_eq!(r["results"]["k_orders"][1]["exprs"]["value"]["v"], prev);

    // ---- the user's default filters ----
    let p = t.ok(s1, "PUT", &format!("/v1/views/{app_id}/prefs"), json!({ "filters": { "region": [r01] } }));
    assert_eq!(p["view"]["my_filters"], json!({ "region": [r01] }));
    assert_eq!(t.ok(admin, "GET", &format!("/v1/views/{app_id}"), J::Null)["view"]["my_filters"], J::Null, "per user");
    let p = t.ok(s1, "PUT", &format!("/v1/views/{app_id}/prefs"), json!({ "filters": null, "favourite": true }));
    assert_eq!(p["view"]["my_filters"], J::Null);
    assert_eq!(p["view"]["favourite"], json!(true));
    let (st, _) = t.call(s1, "PUT", &format!("/v1/views/{app_id}/prefs"), json!({ "filters": [1] }));
    assert_eq!(st, 400);

    // ---- buttons: what the user may run on a record, and why not ----
    let draft = pick(&url, &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE o.\"state\" = 'Draft' AND c.\"salesperson_id\" = {s1} AND c.\"active\" AND r.\"active\" AND EXISTS (SELECT 1 FROM \"order_line\" l WHERE l.\"order_id\" = o.\"id\") ORDER BY o.\"id\" LIMIT 1"));
    let (st, rec) = t.local.record(s1, "Order", draft);
    assert_eq!(st, 200, "{rec}");
    let names = |v: &J| v.as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(names(&rec["actions"]).contains(&"confirm".to_string()), "{rec}");
    let ship = rec["unavailable"].as_array().unwrap().iter().find(|a| a["name"] == json!("ship")).unwrap_or_else(|| panic!("{rec}"));
    assert!(ship["reason"].as_str().unwrap().contains("permission denied"), "{ship}");
    let (_, rec_a) = t.local.record(admin, "Order", draft);
    let ship = rec_a["unavailable"].as_array().unwrap().iter().find(|a| a["name"] == json!("ship")).unwrap_or_else(|| panic!("{rec_a}"));
    assert!(ship["reason"].as_str().unwrap().contains("only from Confirmed (it is Draft)"), "{ship}");
    // the button runs the transition; the cockpit's figures follow
    let sel = json!({ "order": draft });
    let before = t.local.open_app(s1, &cfg, &sel);
    assert_eq!(before["results"]["b_confirm"][0]["id"], json!(draft), "the button reads its record");
    let open0 = kpi(&before, "k_open")["v"].as_u64().unwrap();
    let base = rec["base"].as_u64().unwrap();
    let (st, b) = t.local.tx(s1, &json!({ "base": base, "ops": [{ "op": "action", "entity": "Order", "id": draft, "action": "confirm" }] }), "appc-confirm");
    assert_eq!(st, 200, "{b}");
    let after = t.local.open_app(s1, &cfg, &sel);
    assert_eq!(kpi(&after, "k_open")["v"], json!(open0 + 1), "evaluated again after the commit");
    // refused: shipping is not theirs; confirming again is not from this state
    let (st, b) = t.tx(s1, json!([{ "op": "action", "entity": "Order", "id": draft, "action": "ship" }]), "appc-ship");
    assert_eq!(st, 403, "{b}");
    let (st, _) = t.tx(s1, json!([{ "op": "action", "entity": "Order", "id": draft, "action": "confirm" }]), "appc-confirm2");
    assert_eq!(st, 422);
    // an action with parameters, as the parameter form sends them
    let (st, b) = t.tx(admin, json!([{ "op": "action", "entity": "Region", "id": r01, "action": "review", "args": { "since": "2026-01-01" } }]), "appc-review");
    assert_eq!(st, 200, "{b}");
    let (st, b) = t.tx(admin, json!([{ "op": "action", "entity": "Region", "id": r01, "action": "review", "args": { "since": "soon" } }]), "appc-review2");
    assert_eq!(st, 422, "{b}");
    assert!(b["error"].as_str().unwrap().contains("since"), "the message names the parameter: {b}");
    let model = t.local.model(admin);
    let region = model["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("Region")).unwrap();
    let review = region["actions"].as_array().unwrap().iter().find(|a| a["name"] == json!("review")).unwrap();
    assert_eq!(review["params"], json!([{ "name": "since", "type": "Date", "default": null, "optional": false }]));

    // ---- forms: create and edit, as the user ----
    let mine = pick(&url, &format!("SELECT CAST(c.\"id\" AS TEXT) FROM \"customer\" c JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" = {s1} AND c.\"active\" AND r.\"active\" ORDER BY c.\"id\" LIMIT 1"));
    let theirs = pick(&url, &format!("SELECT CAST(c.\"id\" AS TEXT) FROM \"customer\" c JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" <> {s1} AND (r.\"manager_id\" IS NULL OR r.\"manager_id\" <> {s1}) AND c.\"active\" AND r.\"active\" ORDER BY c.\"id\" LIMIT 1"));
    let (st, b) = t.tx(s1, json!([{ "op": "create", "entity": "Order", "values": { "number": "APPC-1", "customer": mine, "date": "2026-11-20", "priority": "High" } }]), "appc-form1");
    assert_eq!(st, 200, "{b}");
    let (st, b) = t.tx(s1, json!([{ "op": "create", "entity": "Order", "values": { "number": "APPC-2", "customer": theirs, "date": "2026-11-20" } }]), "appc-form2");
    assert_eq!(st, 403, "another salesperson's customer: {b}");
    let (st, b) = t.tx(s1, json!([{ "op": "create", "entity": "Order", "values": { "number": "APPC-3", "customer": mine } }]), "appc-form3");
    assert_eq!(st, 422, "the date is required: {b}");
    assert!(b["error"].as_str().unwrap().contains("date"), "{b}");
    let (st, b) = t.tx(s1, json!([{ "op": "create", "entity": "Order", "values": { "number": "APPC-1", "customer": mine, "date": "2026-11-20" } }]), "appc-form4");
    assert_eq!(st, 422, "unique number: {b}");
    // edit: a stale base is refused with what changed
    let new_id = pick(&url, "SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"number\" = 'APPC-1'");
    let (_, rec) = t.local.record(s1, "Order", new_id);
    let base = rec["base"].as_u64().unwrap();
    assert!(rec["editable"].as_array().unwrap().contains(&json!("priority")), "{rec}");
    let (st, b) = t.local.tx(admin, &json!({ "base": base, "ops": [{ "op": "update", "entity": "Order", "id": new_id, "values": { "priority": "Low" } }] }), "appc-edit1");
    assert_eq!(st, 200, "{b}");
    let (st, b) = t.local.tx(s1, &json!({ "base": base, "ops": [{ "op": "update", "entity": "Order", "id": new_id, "values": { "priority": "Normal" } }] }), "appc-edit2");
    assert_eq!(st, 409, "{b}");
    assert!(b["conflicts"].to_string().contains("priority"), "{b}");
    // a field hidden from the user: not in their model, not editable
    let cust = t.local.model(s1)["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("Customer")).unwrap().clone();
    assert!(!cust["fields"].as_array().unwrap().iter().any(|f| f["name"] == json!("credit_limit")));
    let (st, _) = t.tx(s1, json!([{ "op": "update", "entity": "Customer", "id": mine, "values": { "credit_limit": "1.00 EUR" } }]), "appc-hidden");
    assert!(st == 403 || st == 422, "{st}");

    // ---- configurations: each type's shape is checked ----
    let bad = json!({ "version": 1, "pages": [{ "id": "p1", "components": [
        { "id": "a", "type": "chart", "layout": { "x": 0, "y": 0, "w": 6, "h": 4 }, "config": { "kind": "radar", "source": { "type": "expr", "expr": "" } } },
        { "id": "b", "type": "kpi", "layout": { "x": 0, "y": 4, "w": 6, "h": 4 }, "config": { "source": { "type": "cell" }, "compare": { "kind": "yesterday" } } },
        { "id": "c", "type": "filter", "layout": { "x": 0, "y": 8, "w": 6, "h": 4 }, "config": { "filters": [{ "name": "Region", "entity": "Order", "field": "customer..region", "widget": "slider" }] } },
        { "id": "d", "type": "button", "layout": { "x": 0, "y": 12, "w": 6, "h": 4 }, "config": { "entity": "Order", "action": "confirm", "target": { "from": "somewhere" } } },
        { "id": "e", "type": "form", "layout": { "x": 0, "y": 16, "w": 6, "h": 4 }, "config": { "entity": "Order", "mode": "delete", "columns": 4 } },
        { "id": "f", "type": "view", "layout": { "x": 0, "y": 20, "w": 6, "h": 4 }, "config": { "entity": "Order", "type": "calendar", "select": "Bad Name" } },
        { "id": "g", "type": "view", "layout": { "x": 6, "y": 0, "w": 6, "h": 4 }, "config": { "entity": "Order", "select": "dup" }, "bindings": { "filters": { "Order": { "region": 3 } } } },
        { "id": "h", "type": "view", "layout": { "x": 12, "y": 0, "w": 6, "h": 4 }, "config": { "entity": "Order", "select": "dup" } },
        { "id": "i", "type": "view", "layout": { "x": 0, "y": 24, "w": 6, "h": 4 }, "config": { "entity": "Order", "type": "grid", "edit": { "delete": true, "cells": "yes", "move": true, "colour": true } } },
        { "id": "j", "type": "view", "layout": { "x": 6, "y": 24, "w": 6, "h": 4 }, "config": { "entity": "Order", "type": "kanban", "edit": { "add": true } } },
        { "id": "k", "type": "view", "layout": { "x": 12, "y": 24, "w": 6, "h": 4 }, "config": { "entity": "Order", "edit": "all" } }
    ] }] });
    let (st, b) = t.call(admin, "POST", "/v1/views", json!({ "kind": "app", "name": "Bad", "config": bad }));
    assert_eq!(st, 400, "{b}");
    let d = b["diagnostics"].to_string();
    for (what, why) in [("component a", "`kind`"), ("component a", "`source.expr`"), ("component b", "`source`"), ("component b", "`compare`"), ("component c", "filter 1: `name`"), ("component c", "filter 1: `field`"), ("component c", "filter 1: `widget`"), ("component d", "`target`"), ("component e", "`mode`"), ("component e", "`columns`"), ("component f", "`type`"), ("component f", "`select`"), ("component g", "`bindings.filters.Order`"), ("component h", "two filters or selections named `@dup`"), ("component i", "`edit.delete`: records are not deleted"), ("component i", "`edit.cells`: true or false"), ("component i", "`edit.move`: only a board"), ("component i", "`edit.colour`: unknown"), ("component j", "`edit.add`: only a grid"), ("component k", "`edit`: what may be changed")] {
        assert!(d.contains(&format!("{what}: {why}")), "{what} {why}: {d}");
    }
    // the cockpit itself fits
    let (st, b) = t.call(admin, "POST", "/v1/views", json!({ "kind": "app", "name": "Copy", "config": cfg }));
    assert!(st == 200 || st == 201, "{b}");

    // ---- an embedded view open to editing: the flag opens the grid's editors, the API decides ----
    let editing = json!({ "version": 1, "pages": [{ "id": "p1", "components": [
        { "id": "v", "type": "view", "layout": { "x": 0, "y": 0, "w": 12, "h": 8 }, "config": { "entity": "Order", "type": "grid", "edit": { "cells": true, "add": true } } },
        { "id": "b", "type": "view", "layout": { "x": 12, "y": 0, "w": 12, "h": 8 }, "config": { "entity": "Order", "type": "kanban", "view": { "kanban": { "field": "state" } }, "edit": { "move": true, "cells": false } } }
    ] }] });
    let (st, b) = t.call(admin, "POST", "/v1/views", json!({ "kind": "app", "name": "Editing", "config": editing }));
    assert!(st == 200 || st == 201, "{b}");
    let app2 = b["view"]["id"].as_u64().unwrap();
    t.ok(admin, "PUT", &format!("/v1/views/{app2}/shares"), json!({ "base": b["view"]["version"], "shares": [{ "type": "everyone", "edit": false }] }));
    let (st, b) = t.call(s1, "GET", &format!("/v1/views/{app2}"), J::Null);
    assert_eq!(st, 200, "shared with everyone: {b}");
    assert_eq!(b["view"]["config"]["pages"][0]["components"][0]["config"]["edit"], json!({ "cells": true, "add": true }));
    // a salesperson through it: another salesperson's order, a product, a hidden field, a
    // transition not theirs — each refused by the API whatever the component allows
    let other = pick(&url, &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE c.\"salesperson_id\" <> {s1} AND (r.\"manager_id\" IS NULL OR r.\"manager_id\" <> {s1}) ORDER BY o.\"id\" LIMIT 1"));
    let before = text(&url, &format!("SELECT \"priority\" FROM \"order\" WHERE \"id\" = {other}"));
    let (st, b) = t.tx(s1, json!([{ "op": "update", "entity": "Order", "id": other, "values": { "priority": if before == "Low" { "High" } else { "Low" } } }]), "appc-view-edit1");
    assert!(st == 403 || st == 404, "another salesperson's order: {st} {b}");
    assert_eq!(text(&url, &format!("SELECT \"priority\" FROM \"order\" WHERE \"id\" = {other}")), before, "unchanged");
    let product = pick(&url, "SELECT CAST(\"id\" AS TEXT) FROM \"product\" ORDER BY \"id\" LIMIT 1");
    let (st, b) = t.tx(s1, json!([{ "op": "update", "entity": "Product", "id": product, "values": { "name": "Renamed" } }]), "appc-view-edit2");
    assert_eq!(st, 403, "products are read-only to them: {b}");
    let (st, b) = t.tx(s1, json!([{ "op": "create", "entity": "Region", "values": { "code": "RX", "name": "X" } }]), "appc-view-edit3");
    assert_eq!(st, 403, "no new region (the add option of a grid of regions would not help): {b}");
    let (st, b) = t.tx(s1, json!([{ "op": "update", "entity": "Customer", "id": mine, "values": { "credit_limit": "2.00 EUR" } }]), "appc-view-edit4");
    assert!(st == 403 || st == 422, "a hidden field: {st} {b}");

    // ---- the Excel export with the filters as set ----
    let scope = |f: &J| {
        let params = quadra_server::app_params(&cfg, f);
        let bind = quadra_server::bind_for(&params, &json!({}), true);
        json!({ "params": params, "bind": bind })
    };
    let read = |b: &J| -> Vec<Vec<String>> {
        use calamine::{Reader, Xlsx};
        let bytes = quadra_server::unhex(b["__file"]["hex"].as_str().unwrap()).unwrap();
        let mut x: Xlsx<_> = Xlsx::new(std::io::Cursor::new(bytes)).unwrap();
        let name = x.sheet_names()[0].clone();
        let r = x.worksheet_range(&name).unwrap();
        r.rows().map(|row| row.iter().map(|c| c.to_string()).collect()).collect()
    };
    let enc = |j: &J| j.to_string().replace('%', "%25").replace('&', "%26").replace('=', "%3D");
    let one_region = scope(&json!({ "region": [r01] }));
    let (st, b) = t.call(admin, "GET", &format!("/v1/apps/{app_id}/export.xlsx?component=k_orders&scope={}", enc(&one_region)), J::Null);
    assert_eq!(st, 200, "{b}");
    let rows = read(&b);
    assert_eq!(rows[1][0], "Value");
    let now01 = t.query(admin, json!({ "entity": "Order", "fields": [], "limit": 1, "where": "customer.region.code == \"R01\"" }))["total"].as_u64().unwrap();
    assert_eq!(rows[1][1], now01.to_string(), "the KPI with the region filter: {rows:?}");
    let (st, b) = t.call(admin, "GET", &format!("/v1/apps/{app_id}/export.xlsx?component=c_top&scope={}", enc(&one_region)), J::Null);
    assert_eq!(st, 200, "{b}");
    let rows = read(&b);
    assert_eq!(rows[0], vec!["customer".to_string(), "revenue".to_string()]);
    assert!(rows.len() > 1 && rows.len() <= 11, "{rows:?}");
    let (st, b) = t.call(s1, "GET", &format!("/v1/apps/{app_id}/export.xlsx?component=s_pivot&scope={}", enc(&scope(&json!({})))), J::Null);
    assert_eq!(st, 200, "{b}");
    let rows = read(&b);
    let now_s1 = t.query(s1, json!({ "entity": "Order", "fields": [], "limit": 1 }))["total"].as_u64().unwrap();
    assert_eq!(rows[0][6], now_s1.to_string(), "the salesperson's own orders in the sheet: {:?}", rows[0]);
    let (st, _) = t.call(admin, "GET", &format!("/v1/apps/{app_id}/export.xlsx?component=v_board"), J::Null);
    assert_eq!(st, 404, "a view is not exported");

    let _ = (tag, total);
    drop(t);
    cleanup(target, &url, &root);
}

#[test]
fn components_sqlite() {
    components_on("sqlite");
}

#[test]
fn components_postgres() {
    match std::env::var("QUADRA_TEST_PG") {
        Ok(url) if !url.is_empty() => components_on(&url),
        _ => eprintln!("QUADRA_TEST_PG not set: skipped"),
    }
}
