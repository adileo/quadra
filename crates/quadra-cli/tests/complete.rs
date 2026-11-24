//! Completion of the formulas of apps (`POST /v1/complete`, engine/complete.rs): fields after a
//! `.` with their types (through references, collections and the methods between them),
//! methods with their signature, the app's `@params` with their types, lambda parameters, the
//! values of enums, sheets and the cells they hold, signature help, what a user may not read
//! left out, and the time a request takes. On `examples/stress` (the `tiny` world) with a module
//! added for an enum, a function and a metric, on SQLite and, when `QUADRA_TEST_PG` holds a url,
//! on PostgreSQL.

use quadra_server::Local;
use quadra_store::conn::Conn;
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};

const EXTRA: &str = "\
from sales import Customer, Order


enum Level: Bronze, Silver, Gold


# The level of a customer, by revenue.
fn level_of(c: Customer) -> Level:
    return Gold if c.revenue > 10_000.00 EUR else Bronze


metric orders_in(period: DateRange) -> Int:
    label: \"Orders\"
    owner: \"Sales\"
    definition: \"The orders dated in the period.\"
    value = Order.count(o => o.date in period)
";

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

fn project_copy(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let dir = std::env::temp_dir().join(format!("quadra-compl-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml")) {
            std::fs::copy(&p, dir.join(e.file_name())).unwrap();
        }
    }
    std::fs::write(dir.join("extra.qd"), EXTRA).unwrap();
    dir
}

fn fresh(target: &str, tag: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-compl-{}-{tag}.db", std::process::id()));
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qcompl_{}_{tag}", std::process::id());
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
    let dir = std::env::temp_dir().join(format!("quadra-compl-{}-{tag}-gen", std::process::id()));
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

/// The app's filters, as the client sends them to `/v1/eval`.
fn filters() -> J {
    json!([
        { "name": "region", "label": "Region", "entity": "Order", "field": "customer.region", "widget": "multi", "value": [1] },
        { "name": "period", "entity": "Order", "field": "date", "widget": "date_range", "value": ["2026-01-01", null] },
        { "name": "customer", "entity": "Customer", "widget": "record", "value": null },
        { "name": "amount", "entity": "Order", "field": "total", "widget": "number_range", "value": null },
    ])
}

fn sheets() -> J {
    json!([
        { "id": "s1", "name": "Sheet1", "cells": { "A1": { "v": "5.40 EUR" }, "B1": { "v": "=Order.first()" }, "C1": "=B1.customer", "A2": "12", "A3": "30" } },
        { "id": "s2", "name": "My sheet", "cells": {} },
    ])
}

/// `|` marks the caret.
fn ask(local: &mut Local, user: u64, marked: &str, kind: &str) -> J {
    let pos = marked.find('|').unwrap_or(marked.len());
    let text = marked.replacen('|', "", 1);
    let pos16 = text[..pos].encode_utf16().count();
    let body = json!({ "text": text, "pos": pos16, "kind": kind, "params": filters(), "sheets": sheets(), "sheet": "s1" });
    let (st, b) = local.views(user, "POST", "/v1/complete", &[], &body);
    assert_eq!(st, 200, "{marked}: {b}");
    b
}

fn labels(r: &J) -> Vec<String> {
    r["items"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap().to_string()).collect()
}

fn item<'a>(r: &'a J, label: &str) -> &'a J {
    r["items"].as_array().unwrap().iter().find(|i| i["label"] == json!(label)).unwrap_or_else(|| panic!("no `{label}` in {:?}", labels(r)))
}

fn has(r: &J, label: &str) -> bool {
    labels(r).iter().any(|l| l == label)
}

fn complete_on(target: &str) {
    let tag = if target == "sqlite" { "s" } else { "p" };
    let (root, url) = world(target, tag);
    let user = |login: &str| pick(&url, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = '{login}'"));
    let (admin, s1, l1) = (user("admin"), user("sales0001"), user("logistics01"));
    let mut t = Local::open(&root, &url, 64).unwrap();
    let f = "formula";

    // ---- fields after a dot, with their types ----
    let r = ask(&mut t, admin, "=Order.where(o => o.|", f);
    assert_eq!(r["receiver"], json!("Order"), "{r}");
    assert_eq!(item(&r, "customer")["detail"], json!("Customer"));
    assert_eq!(item(&r, "total")["detail"], json!("Money<EUR>"));
    assert_eq!(item(&r, "total")["kind"], json!("formula"));
    assert_eq!(item(&r, "date")["detail"], json!("Date"));
    assert_eq!(r["from"], json!(20), "the name is written from the caret");
    // with what is typed: the matches, first those starting with it
    let r = ask(&mut t, admin, "=Order.where(o => o.cu|", f);
    assert_eq!(labels(&r)[0], "customer");
    assert_eq!((r["from"].as_u64(), r["to"].as_u64()), (Some(20), Some(22)));
    // a word in the middle is replaced whole
    let r = ask(&mut t, admin, "=Order.first().cu|stomer", f);
    assert_eq!(r["to"], json!(23));

    // through references
    let r = ask(&mut t, admin, "=Order.first().customer.region.|", f);
    assert_eq!(r["receiver"], json!("Region"), "{r}");
    assert_eq!(item(&r, "manager")["detail"], json!("User?"));
    assert!(has(&r, "code") && has(&r, "customers") && !has(&r, "total"));
    // through a collection: its methods and its elements' fields, their values together
    let r = ask(&mut t, admin, "=Region.first().customers.|", f);
    assert_eq!(r["receiver"], json!("[Customer]"), "{r}");
    assert!(has(&r, "where") && has(&r, "sum") && has(&r, "group_by"));
    assert_eq!(item(&r, "revenue")["detail"], json!("[Money<EUR>]"));
    let r = ask(&mut t, admin, "=Region.first().customers.where(c => c.active).first().orders.|", f);
    assert_eq!(r["receiver"], json!("[Order]"), "{r}");
    // methods with their signature and what they give
    let r = ask(&mut t, admin, "=Order.|", f);
    let w = item(&r, "where");
    assert_eq!(w["detail"], json!(".where(x => cond)"));
    assert!(w["doc"].as_str().unwrap().contains("satisfy"), "{w}");
    assert_eq!(w["insert"], json!("where("));
    assert!(!has(&r, "total"), "an entity's records: methods, not fields");
    // values: a text's methods, a date's parts
    let r = ask(&mut t, admin, "=Order.first().number.|", f);
    assert!(has(&r, "upper") && has(&r, "starts_with"), "{r}");
    let r = ask(&mut t, admin, "=Order.first().date.|", f);
    assert!(has(&r, "month") && has(&r, "end_of_month"), "{r}");

    // ---- the app's parameters, typed by what they filter ----
    let r = ask(&mut t, admin, "=@|", f);
    assert_eq!(item(&r, "@region")["detail"], json!("[Region]"), "{r}");
    assert_eq!(item(&r, "@period")["detail"], json!("DateRange"));
    assert_eq!(item(&r, "@customer")["detail"], json!("Customer?"));
    assert_eq!(item(&r, "@amount_min")["detail"], json!("Money<EUR>?"));
    assert!(!has(&r, "@period__on"));
    assert_eq!(r["from"], json!(1), "`@` is part of what is replaced");
    assert_eq!(item(&r, "@region")["doc"], json!("**Region**: the app's multi filter on Order.customer.region"));
    let r = ask(&mut t, admin, "=@region.|", f);
    assert_eq!(r["receiver"], json!("[Region]"));
    assert!(has(&r, "count") && has(&r, "code"));
    let r = ask(&mut t, admin, "=@customer.|", f);
    assert!(has(&r, "orders") && has(&r, "credit_limit"), "{r}");
    let r = ask(&mut t, admin, "=Order.count(o => o.date in @pe|", f);
    assert_eq!(labels(&r)[0], "@period");

    // ---- lambda parameters ----
    let r = ask(&mut t, admin, "=Order.count(o => o.total > 100.00 EUR and |", f);
    assert_eq!(item(&r, "o")["kind"], json!("variable"));
    assert_eq!(item(&r, "o")["detail"], json!("Order"));
    let r = ask(&mut t, admin, "=Order.sum(o => o.lines.sum(l => l.|", f);
    assert_eq!(r["receiver"], json!("OrderLine"), "{r}");
    assert!(has(&r, "amount") && has(&r, "product"));
    let r = ask(&mut t, admin, "=Order.group_by(o => o.customer.region, o => o.date.month).map(g => g.|", f);
    assert_eq!(item(&r, "key")["detail"], json!("Region"), "{r}");
    assert_eq!(item(&r, "key2")["detail"], json!("YearMonth"));
    assert_eq!(item(&r, "items")["detail"], json!("[Order]"));
    let r = ask(&mut t, admin, "=Order.group_by(o => o.state).map(g => g.items.|", f);
    assert!(has(&r, "sum") && has(&r, "total"), "{r}");

    // ---- values of enums: those a comparison expects first ----
    let r = ask(&mut t, admin, "=Order.count(o => o.state is |", f);
    assert_eq!(labels(&r)[..4], ["Draft", "Confirmed", "Shipped", "Cancelled"], "{r}");
    let r = ask(&mut t, admin, "=Order.count(o => o.priority == H|", f);
    assert_eq!(labels(&r)[0], "High");
    let r = ask(&mut t, admin, "=Order.count(o => o.state in {Draft, |", f);
    assert_eq!(labels(&r)[0], "Draft");
    let r = ask(&mut t, admin, "=Level.|", f);
    assert_eq!(labels(&r), ["Bronze", "Silver", "Gold"]);
    let r = ask(&mut t, admin, "=Si|", f);
    assert_eq!(item(&r, "Silver")["detail"], json!("Level"));

    // ---- functions and metrics, with their signature and doc ----
    let r = ask(&mut t, admin, "=lev|", f);
    let l = item(&r, "level_of");
    assert_eq!((l["kind"].as_str(), l["detail"].as_str(), l["insert"].as_str()), (Some("function"), Some("level_of(c: Customer) -> Level"), Some("level_of(")));
    assert!(l["doc"].as_str().unwrap().contains("by revenue"), "{l}");
    let r = ask(&mut t, admin, "=orders|", f);
    let m = item(&r, "orders_in");
    assert_eq!(m["kind"], json!("metric"));
    assert!(m["doc"].as_str().unwrap().contains("dated in the period"), "{m}");

    // ---- sheets, and the cells they hold ----
    let r = ask(&mut t, admin, "=Sh|", f);
    assert_eq!(item(&r, "Sheet1")["insert"], json!("Sheet1!"));
    let r = ask(&mut t, admin, "=My|", f);
    assert_eq!(item(&r, "My sheet")["insert"], json!("'My sheet'!"));
    let r = ask(&mut t, admin, "='My|", f);
    assert_eq!(item(&r, "My sheet")["insert"], json!("'My sheet'!"));
    assert_eq!(r["from"], json!(1));
    // a value's type, a formula's type, through another formula and a range
    let r = ask(&mut t, admin, "=A1.|", f);
    assert_eq!(r["receiver"], json!("Money<EUR>"), "{r}");
    assert!(has(&r, "round") && has(&r, "currency"));
    let r = ask(&mut t, admin, "=Sheet1!B1.|", f);
    assert_eq!(r["receiver"], json!("Order?"), "{r}");
    let r = ask(&mut t, admin, "=C1.|", f);
    assert_eq!(r["receiver"], json!("Customer"), "{r}");
    let r = ask(&mut t, admin, "=A2:A3.|", f);
    assert_eq!(r["receiver"], json!("[Int]"), "{r}");

    // ---- signature help ----
    let r = ask(&mut t, admin, "=Order.sum(|", f);
    assert_eq!(r["signature"]["label"], json!("sum(x => v)"), "{r}");
    assert_eq!(r["signature"]["active"], json!(0));
    let r = ask(&mut t, admin, "=Order.where(o => o.date in DateRange(2026-01-01, |", f);
    assert_eq!(r["signature"]["label"], json!("DateRange(from, to)"), "{r}");
    assert_eq!(r["signature"]["params"], json!(["from", "to"]));
    assert_eq!(r["signature"]["active"], json!(1));
    let r = ask(&mut t, admin, "=orders_in(|", f);
    assert_eq!(r["signature"]["label"], json!("orders_in(period: DateRange) -> Int"), "{r}");
    let r = ask(&mut t, admin, "=Order.first().number.starts_with(|", f);
    assert_eq!(r["signature"]["label"], json!("starts_with(text)"), "{r}");
    let r = ask(&mut t, admin, "=Order.where(o => (o.total > 0.00 EUR or |", f);
    assert_eq!(r["signature"]["label"], json!("where(x => cond)"), "a `(` grouping is not a call: {r}");
    let r = ask(&mut t, admin, "=Order.group_by(o => o.state, |", f);
    assert_eq!(r["signature"]["active"], json!(1));
    assert_eq!(r["signature"]["label"], json!("group_by(x => key, x => key2)"), "{r}");

    // ---- expressions and texts ----
    let r = ask(&mut t, admin, "Order.co|", "expr");
    assert_eq!(labels(&r)[0], "count");
    let r = ask(&mut t, admin, "Orders: {= Order.co| } and more", "template");
    assert_eq!(labels(&r)[0], "count");
    let r = ask(&mut t, admin, "Orders: {= Order.count() } and Ord|", "template");
    assert!(labels(&r).is_empty(), "outside `{{= … }}`: {r}");
    let r = ask(&mut t, admin, "Ord|", f);
    assert!(labels(&r).is_empty(), "a value, not a formula");
    let r = ask(&mut t, admin, "=\"Ord|", f);
    assert!(labels(&r).is_empty(), "in a text");

    // ---- what the user may not read is not offered ----
    let r = ask(&mut t, l1, "=Or|", f);
    assert!(!has(&r, "Order") && !has(&r, "OrderLine"), "{:?}", labels(&r));
    let r = ask(&mut t, l1, "=Re|", f);
    assert!(has(&r, "Region"));
    let r = ask(&mut t, l1, "=Order.|", f);
    assert!(labels(&r).is_empty(), "{r}");
    let r = ask(&mut t, l1, "=Region.first().customers.|", f);
    assert!(has(&r, "count") && !has(&r, "credit_limit") && !has(&r, "name"), "methods, no field of customers: {r}");
    let r = ask(&mut t, s1, "=Customer.first().|", f);
    assert!(has(&r, "name") && !has(&r, "credit_limit"), "a hidden field: {r}");
    let r = ask(&mut t, admin, "=Customer.first().|", f);
    assert!(has(&r, "credit_limit"));

    // ---- bad requests ----
    let (st, _) = t.views(admin, "POST", "/v1/complete", &[], &json!({ "pos": 1 }));
    assert_eq!(st, 400);
    let (st, _) = t.views(admin, "POST", "/v1/complete", &[], &json!({ "text": "=1", "kind": "nope" }));
    assert_eq!(st, 400);
    // unknown parameters: completed without them
    let (st, b) = t.views(admin, "POST", "/v1/complete", &[], &json!({ "text": "=Order.", "params": [{ "name": "x", "entity": "Nope", "widget": "select" }] }));
    assert_eq!(st, 200, "{b}");
    assert!(b["warning"].is_string() && has(&b, "where"));

    // ---- fast: no query of the data, nothing checked again ----
    let cases = ["=Order.where(o => o.customer.region.|", "=Order.group_by(o => o.state).map(g => g.items.sum(o => o.|", "=@region.|", "=C1.|", "=Or|", "=Order.count(o => o.state is |"];
    let mut worst = 0.0f64;
    for _ in 0..5 {
        for c in cases {
            let started = std::time::Instant::now();
            let _ = ask(&mut t, s1, c, f);
            worst = worst.max(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    eprintln!("completion: slowest request {worst:.1} ms");
    assert!(worst < 50.0, "a request took {worst:.1} ms");

    drop(t);
    cleanup(target, &url, &root);
}

#[test]
fn complete_sqlite() {
    complete_on("sqlite");
}

#[test]
fn complete_postgres() {
    match std::env::var("QUADRA_TEST_PG") {
        Ok(url) if !url.is_empty() => complete_on(&url),
        _ => eprintln!("QUADRA_TEST_PG not set: skipped"),
    }
}
