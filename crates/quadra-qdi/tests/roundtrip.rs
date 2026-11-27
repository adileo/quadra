#![recursion_limit = "512"]

//! The round trips of `.qdi` files:
//!
//! - `.qdi → JSON → .qdi` gives the same bytes for a file in canonical form (the Sales
//!   cockpit, a file with every construct);
//! - `JSON → .qdi → JSON` gives the same JSON, up to the normalizations of the interface files reference (quadra.run/reference/interfaces)
//!   ("Round trip"), which `normalize` below applies independently of the crate's code;
//! - what is not canonical is made canonical once (`format` is idempotent).

use quadra_qdi::{format, from_json, line_col, parse, print, to_json};
use serde_json::{json, Map, Value as J};

fn data(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn cockpit_json() -> Vec<J> {
    let path = format!(
        "{}/../../examples/stress/apps/sales-cockpit.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let doc: J = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    doc["views"].as_array().unwrap().clone()
}

fn diags(src: &str) -> Vec<String> {
    parse(src)
        .1
        .iter()
        .map(|d| {
            let (l, c) = line_col(src, d.span.start);
            format!("{l}:{c}: {}", d.message)
        })
        .collect()
}

/// Where two JSON values differ (for the assertion messages).
fn diff(path: &str, a: &J, b: &J, out: &mut Vec<String>) {
    match (a, b) {
        (J::Object(x), J::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => diff(&format!("{path}.{k}"), p, q, out),
                    (p, q) => out.push(format!("{path}.{k}: {p:?} vs {q:?}")),
                }
            }
        }
        (J::Array(x), J::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), p, q, out);
            }
        }
        _ => {
            if a != b {
                out.push(format!("{path}: {a} vs {b}"));
            }
        }
    }
}

fn assert_same(a: &[J], b: &[J]) {
    let mut out = Vec::new();
    diff("", &J::Array(a.to_vec()), &J::Array(b.to_vec()), &mut out);
    assert!(out.is_empty(), "the JSON differs:\n{}", out.join("\n"));
}

// ---- the normalizations of the interface files reference (quadra.run/reference/interfaces), written again from the spec ----------------------------------------

fn drop_nulls(o: &mut Map<String, J>) {
    o.retain(|_, v| !v.is_null());
}

fn drop_if(o: &mut Map<String, J>, k: &str, default: J) {
    if o.get(k) == Some(&default) {
        o.remove(k);
    }
}

fn drop_empty(o: &mut Map<String, J>, keys: &[&str]) {
    for k in keys {
        let empty = match o.get(*k) {
            Some(J::Array(a)) => a.is_empty(),
            Some(J::Object(m)) => m.is_empty(),
            Some(J::String(s)) => s.is_empty(),
            _ => false,
        };
        if empty {
            o.remove(*k);
        }
    }
}

fn normalize_view(v: &mut Map<String, J>) {
    drop_nulls(v);
    drop_empty(
        v,
        &[
            "widths", "where", "order", "columns", "colors", "search", "drill",
        ],
    );
    drop_if(v, "grouped", json!(false));
    drop_if(v, "board", json!(false));
    if let Some(keys) = v
        .get_mut("group")
        .and_then(|g| g.get_mut("keys"))
        .and_then(J::as_array_mut)
    {
        for k in keys {
            if let Some(m) = k.as_object_mut() {
                drop_nulls(m);
            }
        }
    }
}

fn normalize(defs: &[J]) -> Vec<J> {
    defs.iter()
        .map(|d| {
            let mut out = Map::new();
            for k in [
                "kind",
                "entity",
                "type",
                "name",
                "description",
                "config",
                "shares",
            ] {
                out.insert(
                    k.to_string(),
                    d.get(k).cloned().unwrap_or(if k == "description" {
                        json!("")
                    } else {
                        J::Null
                    }),
                );
            }
            // shares with users and teams stay data
            let shares: Vec<J> = d["shares"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| matches!(s["type"].as_str(), Some("role" | "everyone")))
                .cloned()
                .collect();
            out.insert("shares".into(), J::Array(shares));
            let mut cfg = out["config"].clone();
            if d["kind"] == "view" {
                normalize_view(cfg.as_object_mut().unwrap());
            } else {
                let c = cfg.as_object_mut().unwrap();
                c.entry("version").or_insert(json!(1));
                let mut owned: std::collections::HashSet<String> = Default::default();
                let mut defs: Map<String, J> = Map::new();
                for p in c["pages"].as_array().unwrap() {
                    for comp in p["components"].as_array().into_iter().flatten() {
                        for f in comp["config"]["filters"].as_array().into_iter().flatten() {
                            defs.insert(f["name"].as_str().unwrap().into(), f.clone());
                        }
                    }
                }
                for p in c["pages"].as_array_mut().unwrap() {
                    let comps = p["components"].as_array_mut().unwrap();
                    comps.sort_by_key(|c| (c["layout"]["y"].as_u64(), c["layout"]["x"].as_u64()));
                    for comp in comps.iter_mut() {
                        let m = comp.as_object_mut().unwrap();
                        m.entry("bindings").or_insert(json!({}));
                        let ty = m["type"].as_str().unwrap().to_string();
                        let Some(k) = m.get_mut("config").and_then(J::as_object_mut) else {
                            continue;
                        };
                        drop_nulls(k);
                        match ty.as_str() {
                            "button" => {
                                drop_if(k, "label", json!(""));
                                drop_if(k, "style", json!("primary"));
                                drop_empty(k, &["args"]);
                            }
                            "form" => {
                                drop_if(k, "mode", json!("create"));
                                drop_if(k, "columns", json!(2));
                                drop_empty(k, &["sections", "defaults"]);
                            }
                            "view" => {
                                if let Some(v) = k.get_mut("view").and_then(J::as_object_mut) {
                                    if !v.is_empty() {
                                        normalize_view(v);
                                    }
                                }
                            }
                            "sheet" => {
                                for s in k
                                    .get_mut("sheets")
                                    .and_then(J::as_array_mut)
                                    .into_iter()
                                    .flatten()
                                {
                                    if let Some(s) = s.as_object_mut() {
                                        s.entry("cells").or_insert(json!({}));
                                    }
                                }
                            }
                            "filter" => {
                                // the first filter bar (in the grid's order) showing a filter defines it
                                let mut names: Vec<String> = Vec::new();
                                for f in
                                    k.get("filters").and_then(J::as_array).into_iter().flatten()
                                {
                                    names.push(f["name"].as_str().unwrap().into());
                                }
                                for n in k.get("show").and_then(J::as_array).into_iter().flatten() {
                                    names.push(n.as_str().unwrap().into());
                                }
                                let (mut own, mut borrowed) = (Vec::new(), Vec::new());
                                for n in names {
                                    match defs.get(&n) {
                                        Some(d) if owned.insert(n.clone()) => own.push(d.clone()),
                                        _ => borrowed.push(json!(n)),
                                    }
                                }
                                k.remove("filters");
                                k.remove("show");
                                if !own.is_empty() {
                                    k.insert("filters".into(), J::Array(own));
                                }
                                if !borrowed.is_empty() {
                                    k.insert("show".into(), J::Array(borrowed));
                                }
                            }
                            _ => {}
                        }
                        if let Some(b) = m.get_mut("bindings").and_then(J::as_object_mut) {
                            if b.get("filters")
                                .is_some_and(|f| f.as_object().is_some_and(Map::is_empty))
                            {
                                b.remove("filters");
                            }
                        }
                    }
                }
            }
            out.insert("config".into(), cfg);
            J::Object(out)
        })
        .collect()
}

// ---- the Sales cockpit -------------------------------------------------------------------------------------------

#[test]
fn sales_cockpit() {
    let json = cockpit_json();
    let (text, warnings) = from_json(&json).unwrap();
    // the file in the repository is what the JSON prints as
    let golden = data("sales-cockpit.qdi");
    assert_eq!(
        text, golden,
        "examples/stress/apps/sales-cockpit.json no longer prints as tests/data/sales-cockpit.qdi"
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}"); // its owner
    assert!(diags(&golden).is_empty(), "{:?}", diags(&golden));
    // .qdi → JSON → .qdi: the same bytes
    let back = to_json(&golden).unwrap();
    assert_eq!(from_json(&back).unwrap().0, golden);
    // JSON → .qdi → JSON: the same JSON, normalized
    assert_same(&back, &normalize(&json));
    // the formulas and the layout as the server reads them
    let comps = back[0]["config"]["pages"][0]["components"]
        .as_array()
        .unwrap();
    assert_eq!(comps.len(), 13);
    let b = comps.iter().find(|c| c["id"] == "b_confirm").unwrap();
    assert_eq!(
        b["config"]["target"],
        json!({ "from": "param", "name": "order" })
    );
    assert_eq!(b["layout"], json!({ "x": 16, "y": 14, "w": 8, "h": 2 }));
}

// ---- a file with every construct -------------------------------------------------------------------------------

#[test]
fn everything_is_canonical() {
    let src = data("everything.qdi");
    assert!(diags(&src).is_empty(), "{:?}", diags(&src));
    // the printer writes the tree it reads
    let (file, _) = parse(&src);
    assert_eq!(print(&file), src);
    // .qdi → JSON → .qdi
    let json = to_json(&src).unwrap();
    assert_eq!(from_json(&json).unwrap().0, src);
    // JSON → .qdi → JSON
    let again = to_json(&from_json(&json).unwrap().0).unwrap();
    assert_same(&again, &json);
    assert_same(&normalize(&json), &json);
}

#[test]
fn everything_means_what_it_says() {
    let json = to_json(&data("everything.qdi")).unwrap();
    let app = &json[0];
    assert_eq!(
        app["shares"],
        json!([{ "type": "role", "name": "Admin", "edit": true }, { "type": "role", "name": "Sales", "edit": false }, { "type": "everyone", "edit": false }])
    );
    assert_eq!(app["config"]["theme"], "dark");
    assert_eq!(app["config"]["lets"][1]["expr"], "sold.group_by(o => o.date.start_of_month()).map(g => (month: g.key, revenue: g.items.sum(o => o.total)))");
    let comps: Vec<&J> = app["config"]["pages"][0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .collect();
    let c = |id: &str| {
        *comps
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("no {id}"))
    };
    // filters: defined by the first bar showing them, borrowed by the others
    assert_eq!(c("bar")["config"]["filters"].as_array().unwrap().len(), 4);
    assert_eq!(c("side")["config"]["show"], json!(["region"]));
    assert_eq!(
        c("side")["config"]["filters"][1]["extra"],
        json!({ "a": 1, "b c": [1, 2.5, null] })
    );
    // layout
    assert_eq!(
        c("k3")["layout"],
        json!({ "x": 14, "y": 2, "w": 4, "h": 3 })
    );
    assert_eq!(
        c("b4")["layout"],
        json!({ "x": 15, "y": 21, "w": 4, "h": 2 })
    );
    assert_eq!(
        c("f1")["layout"],
        json!({ "x": 10, "y": 23, "w": 14, "h": 2 })
    );
    assert_eq!(
        c("floating")["layout"],
        json!({ "x": 2, "y": 50, "w": 6, "h": 3 })
    );
    assert_eq!(
        c("g1")["layout"],
        json!({ "x": 0, "y": 54, "w": 6, "h": 4 })
    );
    // components in the grid's order
    let ys: Vec<(u64, u64)> = comps
        .iter()
        .map(|c| {
            (
                c["layout"]["y"].as_u64().unwrap(),
                c["layout"]["x"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut sorted = ys.clone();
    sorted.sort();
    assert_eq!(ys, sorted);
    // sources, runs, sections, custom forms
    assert_eq!(
        c("k2")["config"]["source"],
        json!({ "type": "cell", "component": "grid1", "cell": "Sheet1!B2" })
    );
    assert_eq!(c("k4")["config"]["source"]["expr"], "Order.cnt((");
    assert_eq!(
        c("c2")["config"]["source"],
        json!({ "type": "range", "component": "grid1", "range": "Sheet1!A1:C10" })
    );
    assert_eq!(
        c("b2")["config"]["target"],
        json!({ "from": "param", "name": "region" })
    );
    assert_eq!(c("b2")["config"]["entity"], "Customer");
    assert_eq!(c("b4")["config"].get("target"), None);
    assert_eq!(
        c("b5")["config"]["target"],
        json!({ "from": "cell", "component": "grid1", "cell": "A1" })
    );
    assert_eq!(
        c("f1")["config"]["defaults"]["date"],
        json!({ "from": "value", "value": "2026-01-01" })
    );
    let m = &c("move_stock")["config"];
    assert_eq!(m["mode"], "custom");
    assert_eq!(m["submit"], "let t = Transfer(product=product, qty=qty, to=to, note=note)\nt.confirm()\n\n# the record the form publishes\nt");
    assert_eq!(
        m["select"],
        json!({ "name": "transfer", "entity": "Transfer" })
    );
    assert_eq!(c("quick")["config"]["fields"][0]["where"], "1..100");
    // a sheet's formulas with references, kept as written
    let cells = &c("grid1")["config"]["sheets"][0]["cells"];
    assert_eq!(cells["B2"]["v"], "='Q4 plan'!A1 + Sheet1!B1 * $A$1");
    assert_eq!(cells["C2"]["v"], "=A2#");
    assert_eq!(cells["D2"]["v"], "=A1 +");
    assert_eq!(cells["E1"], json!({ "s": {} }));
    assert_eq!(cells["A3"], json!({}));
    // a text block
    assert_eq!(
        app["config"]["pages"][1]["components"][0]["config"]["text"],
        "## A story\nOver two lines, with \"quotes\" and \\ a backslash."
    );
    // saved views
    assert_eq!(json[1]["config"]["comment"], "Orders waiting to be shipped");
    assert_eq!(json[2]["type"], "kanban");
    assert_eq!(
        json[2]["config"]["kanban"],
        json!({ "field": "state", "cards": ["customer", "total"], "hidden": [], "collapsed": [], "wip": {}, "aggregate": "total" })
    );
}

// ---- JSON as the UI writes it, and shapes without a syntax ---------------------------------------------------------

fn synthetic() -> Vec<J> {
    vec![
        json!({
            "kind": "app", "entity": null, "type": "app", "name": "Odd shapes", "description": "", "owner": "admin",
            "shares": [{ "type": "user", "login": "sales0001", "edit": true }, { "type": "team", "name": "North", "edit": false }, { "type": "role", "name": "Sales", "edit": false }],
            "config": {
                "pages": [{
                    "id": "p-1", "name": "Page 1", "extra": [1, 2],
                    "components": [
                        // as the client writes a new button: every key, nulls included
                        { "id": "cbtn001", "type": "button", "layout": { "x": 0, "y": 0, "w": 6, "h": 2 }, "bindings": {},
                          "config": { "label": "", "entity": null, "action": null, "target": null, "args": {}, "confirm": null, "style": "primary" } },
                        { "id": "c-weird", "type": "button", "title": 12, "layout": { "x": 6, "y": 0, "w": 6, "h": 2 }, "bindings": { "filters": { "Order": { "region": 3 } }, "links": ["x"] },
                          "config": { "label": "Go", "entity": "Order", "action": "confirm", "target": { "from": "param", "name": "nope" }, "args": { "x": { "from": "magic" } }, "style": "loud" } },
                        { "id": "form1", "type": "form", "layout": { "x": 12, "y": 0, "w": 12, "h": 4 }, "bindings": {},
                          "config": { "entity": "Order", "mode": "create", "record": null, "sections": [{ "fields": ["number"], "hint": "x" }], "columns": 2, "defaults": { "date": { "from": "value", "value": { "d": 1 } } }, "submit_label": null } },
                        { "id": "v_full", "type": "view", "layout": { "x": 0, "y": 2, "w": 12, "h": 6 }, "bindings": { "filters": {} },
                          "config": { "entity": "Order", "type": "groups", "select": null, "view": {
                              "fields": [], "widths": {}, "where": "", "order": [], "columns": { "x y": "1" }, "colors": [], "known": ["number"],
                              "group": { "keys": [{ "field": "date", "bucket": "month" }, { "field": "state", "bucket": null }], "aggregates": {}, "pivot": false, "order": { "by": "key", "desc": false }, "measure": "count" },
                              "grouped": true, "drill": [{ "field": "state", "value": "Draft" }], "board": false, "kanban": null } } },
                        { "id": "v_part", "type": "view", "layout": { "x": 12, "y": 4, "w": 12, "h": 4 }, "bindings": {},
                          "config": { "entity": "Order", "type": "kanban", "view": { "board": true, "kanban": { "field": "state", "cards": [] } } } },
                        { "id": "v_empty", "type": "view", "layout": { "x": 0, "y": 8, "w": 4, "h": 3 }, "bindings": {}, "config": { "entity": "Order", "view": {} } },
                        { "id": "k_bad", "type": "kpi", "layout": { "x": 4, "y": 8, "w": 4, "h": 3 }, "bindings": {},
                          "config": { "source": { "type": "expr", "expr": "  Order.count()  # hm" }, "format": { "style": "money", "decimals": 2, "currency": "EUR" }, "compare": { "kind": "previous_period", "filter": "per iod" }, "sparkline": "a\nb" } },
                        { "id": "md", "type": "markdown", "layout": { "x": 8, "y": 8, "w": 4, "h": 3 }, "config": { "text": "line\r\nwindows" } },
                        { "id": "s1", "type": "sheet", "layout": { "x": 12, "y": 8, "w": 12, "h": 6 }, "bindings": {},
                          "config": { "active": "fixed", "sheets": [
                              { "id": "fixed", "name": "S", "rows": 5, "cells": { "A1": "plain string", "B1": { "v": 42 }, "C1": { "v": "=Order.count()", "note": "x" }, "a1": { "v": "lower" }, "D1": { "s": { "fmt": { "kind": "number", "thou": false } }, "v": "x" } }, "merges": ["A1:B1", "bad"], "freeze": { "rows": 1, "cols": "2" } },
                              { "id": "x", "name": 5 }
                          ] } },
                        { "id": "f_c", "type": "form", "layout": { "x": 0, "y": 11, "w": 6, "h": 3 }, "bindings": {},
                          "config": { "mode": "custom", "fields": [{ "name": "n", "type": "Int", "where": "x default y" }], "submit": "  indented\ncode", "after": "select", "select": { "name": "x" } } },
                        { "id": "f_d", "type": "form", "layout": { "x": 6, "y": 11, "w": 6, "h": 3 }, "bindings": {},
                          "config": { "mode": "custom", "submit": "x.do()", "submit_label": "Do", "after": "clear" } },
                        { "id": "u1", "type": "filters", "layout": { "x": 0, "y": 14, "w": 4, "h": 2 }, "bindings": {}, "config": 7 },
                        { "id": "ch", "type": "chart", "layout": { "x": 4, "y": 14, "w": 8, "h": 2 }, "bindings": {}, "config": { "kind": "radar", "source": { "type": "expr", "expr": "@x.count()", "extra": 1 } } },
                        { "id": "ov1", "type": "markdown", "layout": { "x": 12, "y": 14, "w": 6, "h": 4 }, "bindings": {}, "config": { "text": "a" } },
                        { "id": "ov2", "type": "markdown", "layout": { "x": 12, "y": 14, "w": 6, "h": 4 }, "bindings": {}, "config": { "text": "b" } }
                    ]
                }],
                "lets": [{ "name": "x", "expr": "1", "unknown": true }]
            }
        }),
        json!({
            "kind": "view", "entity": "Order", "type": "grid", "name": "Plain", "description": "d", "shares": [],
            "config": { "fields": ["number", "customer"], "widths": { "number": 120 }, "where": "total > 0", "order": [{ "field": "date", "desc": true }], "columns": {}, "colors": [{ "expr": "true", "color": "red", "x": 1 }], "search": "", "group": null, "grouped": false, "known": ["number", "customer"], "board": false, "kanban": null }
        }),
    ]
}

#[test]
fn json_shapes_round_trip() {
    let json = synthetic();
    let (text, warnings) = from_json(&json).unwrap();
    if std::env::var("QDI_SHOW").is_ok() {
        println!("{text}");
    }
    assert!(
        warnings.iter().any(|w| w.contains("user"))
            && warnings.iter().any(|w| w.contains("team"))
            && warnings.iter().any(|w| w.contains("owner")),
        "{warnings:?}"
    );
    assert!(diags(&text).is_empty(), "{:?}\n{text}", diags(&text));
    let back = to_json(&text).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
    assert_same(&back, &normalize(&json));
    // the canonical text prints again as itself
    assert_eq!(from_json(&back).unwrap().0, text);
    assert_eq!(format(&text).unwrap(), text);
    // what has no syntax is kept as `config`/`meta` lines
    for needle in [
        "meta id: \"c-weird\"",
        "meta title: 12",
        "meta bindings: {filters: {Order: {region: 3}}, links: [x]}",
        "config style: loud",
        "config view.drill: [{field: state, value: Draft}]",
        "config view.columns: {\"x y\": \"1\"}",
        "config view: {}",
        "source: raw \"  Order.count()  # hm\"",
        "config format.currency: EUR",
        "config compare: {filter: \"per iod\", kind: previous_period}",
        "sparkline: raw \"a\\nb\"",
        "\"line\\r\\nwindows\"",
        "meta cells.A1: \"plain string\"",
        "meta cells.B1: {v: 42}",
        "meta id: fixed",
        "config submit: \"  indented\\ncode\"",
        "component filters u1",
        "meta config: 7",
        "config kind: radar",
        "at 12, 14 size 6 x 4",
        "meta lets: [{expr: \"1\", name: x, unknown: true}]",
        "config colors: [{color: red, expr: \"true\", x: 1}]",
        "submit \"Do\":",
    ] {
        assert!(text.contains(needle), "`{needle}` not in:\n{text}");
    }
}

// ---- hand-written files ---------------------------------------------------------------------------------------------

#[test]
fn sizes_left_out_are_shared() {
    let src = "app \"A\":\n    page p:\n        row height 4:\n            kpi a:\n                source: 1\n            kpi b width 10:\n                source: 2\n            column:\n                kpi c:\n                    source: 3\n                kpi d height 1:\n                    source: 4\n                kpi e:\n                    source: 5\n        row:\n            kpi f height 3:\n                source: 6\n";
    assert!(diags(src).is_empty(), "{:?}", diags(src));
    let json = to_json(src).unwrap();
    let l = |id: &str| {
        json[0]["config"]["pages"][0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()["layout"]
            .clone()
    };
    // 24 - 10 = 14 for two: 7 each
    assert_eq!(l("a"), json!({ "x": 0, "y": 0, "w": 7, "h": 4 }));
    assert_eq!(l("b"), json!({ "x": 7, "y": 0, "w": 10, "h": 4 }));
    // the column: 4 - 1 = 3 for two: 2 and 1
    assert_eq!(l("c"), json!({ "x": 17, "y": 0, "w": 7, "h": 2 }));
    assert_eq!(l("d"), json!({ "x": 17, "y": 2, "w": 7, "h": 1 }));
    assert_eq!(l("e"), json!({ "x": 17, "y": 3, "w": 7, "h": 1 }));
    // a row as high as its highest child
    assert_eq!(l("f"), json!({ "x": 0, "y": 4, "w": 24, "h": 3 }));
    let canonical = format(src).unwrap();
    assert!(
        canonical.contains("kpi a width 7:") && canonical.contains("row height 3:"),
        "{canonical}"
    );
    assert_eq!(format(&canonical).unwrap(), canonical);
}

#[test]
fn comments_belong_to_the_next_declaration() {
    let src = "# the app\napp \"A\":\n    # the filter\n    filter s: multi Order.state  # trailing\n    page p:\n        # first row, first component\n        row height 2:\n            filters f width 24:\n                show: s\n                # inside, before nothing\n# at the end\n";
    assert!(diags(src).is_empty(), "{:?}", diags(src));
    let out = format(src).unwrap();
    assert_eq!(out, "# the app\napp \"A\":\n    # the filter\n    filter s: multi Order.state\n\n    # trailing\n    page p:\n        row height 2:\n            # first row, first component\n            # inside, before nothing\n            # at the end\n            filters f width 24:\n                show: s\n");
    assert_eq!(format(&out).unwrap(), out);
}

#[test]
fn long_chains_are_cut_and_joined() {
    let src = "app \"A\":\n    let a = Order.where(o => o.state is Confirmed)\n               .group_by(o => o.customer)\n               .map(g => (g.key, g.items.count()))\n    let b = f(\n        1,\n        2\n    )\n";
    let (file, d) = parse(src);
    assert!(d.is_empty(), "{d:?}");
    let quadra_qdi::ast::Item::App(app) = &file.items[0] else {
        panic!()
    };
    assert_eq!(app.lets[0].expr.text, "Order.where(o => o.state is Confirmed).group_by(o => o.customer).map(g => (g.key, g.items.count()))");
    assert_eq!(app.lets[1].expr.text, "f(1, 2)");
    // each token of the text knows its place in the file
    let (t, f) = app.lets[0].expr.map[app.lets[0].expr.map.len() - 1];
    assert_eq!(&app.lets[0].expr.text[t as usize..t as usize + 1], ")");
    assert_eq!(&src[f as usize..f as usize + 1], ")");
}

// ---- problems --------------------------------------------------------------------------------------------------------

#[test]
fn problems_are_reported_at_their_line() {
    let src = "app \"A\":\n    page p:\n        row height 2:\n            kpi a width 30:\n                source: Order.count(\n                colour: red\n            button b width 4:\n                run: @nobody.confirm()\n            chart c:\n                kind: radar\n        kpi floating:\n            source: 1\n";
    let d = diags(src);
    assert!(
        d.iter()
            .any(|m| m.starts_with("6:17: unknown option `colour`")),
        "{d:?}"
    );
    assert!(
        d.iter()
            .any(|m| m.starts_with("10:23: `kind`: one of bar, line")),
        "{d:?}"
    );
    assert!(
        d.iter()
            .any(|m| m.starts_with("11:9: a component of a page is in a `row`")),
        "{d:?}"
    );
    // the expression left open swallows nothing after its line
    let src2 = "app \"A\":\n    page p:\n        row height 2:\n            kpi a width 30:\n                source: Order.count() +\n            button b width 4:\n                run: @nobody.confirm()\n";
    let d2 = diags(src2);
    assert!(!d2.is_empty());
    // the layout and the names are checked when the file becomes JSON
    let src3 = "app \"A\":\n    page p:\n        row height 2:\n            kpi a width 30:\n                source: 1\n            button b width 4:\n                run: @nobody.confirm()\n";
    assert!(diags(src3).is_empty(), "{:?}", diags(src3));
    let e = to_json(src3).unwrap_err();
    let msgs: Vec<String> = e
        .iter()
        .map(|d| {
            let (l, c) = line_col(src3, d.span.start);
            format!("{l}:{c}: {}", d.message)
        })
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("3:9: the row is 24 columns wide: its parts take 34")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("7:22: `@nobody` is not a record selected in the app")),
        "{msgs:?}"
    );
}

#[test]
fn unknown_filters_and_duplicates() {
    let src = "app \"A\":\n    filter s: multi Order.state\n    filter s: multi Order.state\n    filter t: chooser Order.state\n    page p:\n        row height 2:\n            kpi a width 4:\n                source: 1\n            kpi a width 4:\n                source: 2\n";
    let d = diags(src);
    assert!(d.iter().any(|m| m.starts_with("4:15: a widget")), "{d:?}");
    let src = "app \"A\":\n    filter s: multi Order.state\n    filter s: multi Order.state\n    page p:\n        row height 2:\n            kpi a width 4:\n                source: 1\n            kpi a width 4:\n                source: 2\n";
    let e: Vec<String> = to_json(src)
        .unwrap_err()
        .into_iter()
        .map(|d| d.message)
        .collect();
    assert!(
        e.iter().any(|m| m.contains("two filters named `s`")),
        "{e:?}"
    );
    assert!(
        e.iter().any(|m| m.contains("two components named `a`")),
        "{e:?}"
    );
    assert!(
        e.iter().any(|m| m.contains("the filter `s` is not shown")),
        "{e:?}"
    );
}

#[test]
fn no_panics_on_broken_files() {
    // every prefix of the files, and the files with a line removed: problems, never a panic
    for name in ["everything.qdi", "sales-cockpit.qdi"] {
        let src = data(name);
        for k in (0..src.len())
            .filter(|k| src.is_char_boundary(*k))
            .step_by(3)
        {
            let part = &src[..k];
            let (file, d) = parse(part);
            if d.is_empty() {
                if let Ok(json) = quadra_qdi::lower(&file) {
                    let (text, _) = from_json(&json).unwrap();
                    assert_eq!(format(&text).unwrap(), text, "{name} up to {k}");
                }
            }
        }
        let lines: Vec<&str> = src.lines().collect();
        for k in 0..lines.len() {
            let mut l = lines.clone();
            l.remove(k);
            let _ = format(&(l.join("\n") + "\n"));
        }
    }
}

/// `QDI_REGEN=1 cargo test -p quadra-qdi --test roundtrip regenerate`: writes the golden file
/// of the Sales cockpit again (after a change of the printer or of the example).
#[test]
fn regenerate() {
    if std::env::var("QDI_REGEN").is_err() {
        return;
    }
    let (text, _) = from_json(&cockpit_json()).unwrap();
    std::fs::write(
        format!(
            "{}/tests/data/sales-cockpit.qdi",
            env!("CARGO_MANIFEST_DIR")
        ),
        text,
    )
    .unwrap();
}
