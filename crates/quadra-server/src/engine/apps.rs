//! Apps (dashboards): pages of components on a grid, made in the web client and shared like
//! saved views (`kind = "app"` in the same tables, the same API: engine/views.rs). An app is
//! a configuration, never data: what it shows is computed as whoever opens it.
//!
//! The configuration (`config`):
//!
//! ```json
//! { "version": 1,
//!   "pages": [{ "id": "p1", "name": "Page 1",
//!     "components": [{ "id": "c1", "type": "sheet", "title": "…",
//!                      "layout": {"x": 0, "y": 0, "w": 12, "h": 8},
//!                      "config": {…}, "bindings": {…} }] }] }
//! ```
//!
//! **The component contract** (for the types to come: charts, filters, buttons, forms,
//! embedded views, KPI cards): a component is `{id, type, title?, layout, config, bindings}`.
//! The server checks the envelope of every component (ids, type name, layout on the 24-column
//! grid) and the `config` of the types it knows (`sheet`, `markdown`: `component_problems`);
//! the `config` and `bindings` of other types are kept as they are, and checked by whatever
//! evaluates them (`POST /v1/eval` checks every formula it is given, as the user). A new type
//! adds its arm to `component_problems` and, when it computes something, uses `/v1/eval` (or
//! `/v1/query`) as the user: nothing an app holds grants access to data.

use super::sheets::{parse_a1, MAX_COLS, MAX_ROWS};
use super::*;
use std::collections::{BTreeMap, HashSet};

/// The largest configuration of an app (the request body is at most 2 MiB).
pub const MAX_APP_CONFIG: usize = 1_500_000;
const MAX_PAGES: usize = 20;
const MAX_COMPONENTS: usize = 200;
const GRID_COLUMNS: i64 = 24;

fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// What is wrong with an app's configuration (empty: it fits).
pub fn app_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config` must be an object".into()];
    }
    if cfg.to_string().len() > MAX_APP_CONFIG {
        return vec![format!(
            "`config` is too large (at most {} KB)",
            MAX_APP_CONFIG / 1000
        )];
    }
    let Some(pages) = cfg["pages"].as_array() else {
        return vec!["`pages`: a list of pages".into()];
    };
    if pages.len() > MAX_PAGES {
        out.push(format!("at most {MAX_PAGES} pages"));
    }
    let mut page_ids = HashSet::new();
    let mut comp_ids = HashSet::new();
    let mut param_names: HashSet<String> = HashSet::new();
    for (pk, p) in pages.iter().enumerate() {
        let pid = p["id"].as_str().unwrap_or("");
        let at = format!("page {}", pk + 1);
        if !id_ok(pid) || !page_ids.insert(pid.to_string()) {
            out.push(format!(
                "{at}: `id` must be unique, letters, digits, - and _ (at most 40)"
            ));
        }
        if p["name"].as_str().is_some_and(|n| n.chars().count() > 100) {
            out.push(format!("{at}: a name of at most 100 characters"));
        }
        let comps = match &p["components"] {
            J::Null => continue,
            J::Array(a) => a,
            _ => {
                out.push(format!("{at}: `components`: a list"));
                continue;
            }
        };
        if comps.len() > MAX_COMPONENTS {
            out.push(format!("{at}: at most {MAX_COMPONENTS} components"));
        }
        for (ck, c) in comps.iter().enumerate() {
            let cid = c["id"].as_str().unwrap_or("");
            let at = format!(
                "{at}, component {}",
                if id_ok(cid) {
                    cid.to_string()
                } else {
                    (ck + 1).to_string()
                }
            );
            if !id_ok(cid) || !comp_ids.insert(cid.to_string()) {
                out.push(format!(
                    "{at}: `id` must be unique in the app, letters, digits, - and _ (at most 40)"
                ));
            }
            let ty = c["type"].as_str().unwrap_or("");
            if ty.is_empty()
                || ty.len() > 30
                || !ty.chars().all(|x| x.is_ascii_lowercase() || x == '_')
            {
                out.push(format!("{at}: `type`: lower-case letters and _"));
            }
            let l = &c["layout"];
            let n = |k: &str| l[k].as_i64();
            match (n("x"), n("y"), n("w"), n("h")) {
                (Some(x), Some(y), Some(w), Some(h)) if x >= 0 && y >= 0 && (1..=GRID_COLUMNS).contains(&w) && x + w <= GRID_COLUMNS && (1..=200).contains(&h) && y <= 10_000 => {}
                _ => out.push(format!("{at}: `layout`: whole numbers x ≥ 0, y ≥ 0, 1 ≤ w ≤ 24 with x + w ≤ 24, 1 ≤ h ≤ 200")),
            }
            if c["title"].as_str().is_some_and(|t| t.chars().count() > 200) {
                out.push(format!("{at}: a title of at most 200 characters"));
            }
            if !(c["bindings"].is_null() || c["bindings"].is_object()) {
                out.push(format!("{at}: `bindings`: an object"));
            }
            for p in component_problems(ty, &c["config"]) {
                out.push(format!("{at}: {p}"));
            }
            for p in bindings_problems(&c["bindings"]) {
                out.push(format!("{at}: {p}"));
            }
            // the app's parameters: the filters of filter components, the selections of views
            let names: Vec<String> = match ty {
                "filter" => c["config"]["filters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f["name"].as_str().map(str::to_string))
                    .collect(),
                "view" => c["config"]["select"]
                    .as_str()
                    .map(|n| vec![n.to_string()])
                    .unwrap_or_default(),
                "form" if c["config"]["after"] == json!("select") => c["config"]["select"]["name"]
                    .as_str()
                    .map(|n| vec![n.to_string()])
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            for n in names {
                if !param_names.insert(n.clone()) {
                    out.push(format!(
                        "{at}: two filters or selections named `@{n}` in the app"
                    ));
                }
            }
        }
    }
    out.truncate(50);
    out
}

fn bindings_problems(b: &J) -> Vec<String> {
    let mut out = Vec::new();
    match &b["filters"] {
        J::Null => {}
        J::Object(o) => {
            for (k, v) in o {
                if k == "auto" {
                    if !v.is_boolean() {
                        out.push("`bindings.filters.auto`: true or false".into());
                    }
                    continue;
                }
                match v {
                    J::Object(m)
                        if m.values()
                            .all(|p| p.is_null() || p.as_str().is_some_and(|p| p.len() <= 200))
                            && m.keys().all(|n| super::params::name_ok(n)) => {}
                    _ => out.push(format!(
                        "`bindings.filters.{k}`: filter name → field path (or null)"
                    )),
                }
            }
        }
        _ => out.push("`bindings.filters`: an object entity → {filter: path}".into()),
    }
    out
}

/// The `config` of a component of a type the server knows; others are kept as they are.
pub fn component_problems(ty: &str, cfg: &J) -> Vec<String> {
    match ty {
        "sheet" => workbook_problems(cfg),
        "markdown" => match &cfg["text"] {
            J::Null => Vec::new(),
            J::String(t) if t.len() <= 100_000 => Vec::new(),
            _ => vec!["`text`: at most 100 000 characters".into()],
        },
        "chart" => chart_problems(cfg),
        "kpi" => kpi_problems(cfg),
        "filter" => filter_problems(cfg),
        "button" => button_problems(cfg),
        "form" => form_problems(cfg),
        "view" => view_problems(cfg),
        _ => {
            if cfg.is_null() || cfg.is_object() {
                Vec::new()
            } else {
                vec!["`config`: an object".into()]
            }
        }
    }
}

// ---- the configurations of the components that act on the data ------------------------------
//
// Only their shape is checked here: entities, fields, actions and formulas are the model's,
// checked when they are used (as the user who uses them: `/v1/eval`, `/v1/query`, `/v1/tx`).

fn text_ok(j: &J, max: usize) -> bool {
    j.is_null() || j.as_str().is_some_and(|t| t.chars().count() <= max)
}

fn ident_ok(j: &J) -> bool {
    j.as_str().is_some_and(|t| {
        !t.is_empty() && t.len() <= 100 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn path_ok(j: &J) -> bool {
    j.as_str().is_some_and(|t| {
        !t.is_empty()
            && t.len() <= 200
            && t.split('.').count() <= 5
            && t.split('.')
                .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// A formula of a component: one line of at most 4000 characters.
fn formula_ok(j: &J) -> bool {
    j.as_str()
        .is_some_and(|t| !t.trim().is_empty() && t.len() <= 4000 && !t.contains('\n'))
}

/// Where a component takes a record or a value from: a parameter (`@name`), a fixed id, a
/// cell of a spreadsheet of the page.
fn from_problems(what: &str, j: &J, allow_value: bool) -> Vec<String> {
    match j["from"].as_str() {
        Some("param") if super::params::name_ok(j["name"].as_str().unwrap_or("")) => Vec::new(),
        Some("id") if j["id"].as_u64().is_some() => Vec::new(),
        Some("cell")
            if id_ok(j["component"].as_str().unwrap_or(""))
                && j["cell"].as_str().is_some_and(|c| c.len() <= 140) =>
        {
            Vec::new()
        }
        Some("value") if allow_value && j["value"].to_string().len() <= 10_000 => Vec::new(),
        _ => vec![format!(
            "`{what}`: {{from: \"param\", name}}, {{from: \"id\", id}}{}{}",
            ", {from: \"cell\", component, cell}",
            if allow_value {
                " or {from: \"value\", value}"
            } else {
                ""
            }
        )],
    }
}

fn source_problems(src: &J, cell_kind: &str) -> Vec<String> {
    match src["type"].as_str() {
        Some("expr") if formula_ok(&src["expr"]) => Vec::new(),
        Some("expr") => {
            vec!["`source.expr`: a formula of one line, at most 4000 characters".into()]
        }
        Some(k)
            if k == cell_kind
                && id_ok(src["component"].as_str().unwrap_or(""))
                && src[if k == "range" { "range" } else { "cell" }]
                    .as_str()
                    .is_some_and(|r| !r.is_empty() && r.len() <= 140) =>
        {
            Vec::new()
        }
        _ => vec![format!(
            "`source`: {{type: \"expr\", expr}} or {{type: \"{cell_kind}\", component, {}}}",
            if cell_kind == "range" {
                "range"
            } else {
                "cell"
            }
        )],
    }
}

pub fn chart_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    if !matches!(
        cfg["kind"].as_str(),
        Some("bar" | "line" | "area" | "pie" | "scatter")
    ) {
        out.push("`kind`: bar, line, area, pie or scatter".into());
    }
    out.extend(source_problems(&cfg["source"], "range"));
    if !(cfg["max_points"].is_null()
        || cfg["max_points"]
            .as_u64()
            .is_some_and(|n| (1..=5000).contains(&n)))
    {
        out.push("`max_points`: 1 to 5000".into());
    }
    for k in ["stacked", "horizontal", "legend"] {
        if !(cfg[k].is_null() || cfg[k].is_boolean()) {
            out.push(format!("`{k}`: true or false"));
        }
    }
    for k in ["x_title", "y_title", "format"] {
        if !text_ok(&cfg[k], 100) {
            out.push(format!("`{k}`: at most 100 characters"));
        }
    }
    out
}

pub fn kpi_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    out.extend(source_problems(&cfg["source"], "cell"));
    if !text_ok(&cfg["label"], 200) {
        out.push("`label`: at most 200 characters".into());
    }
    match &cfg["compare"] {
        J::Null => {}
        c => match c["kind"].as_str() {
            Some("previous_period")
                if super::params::name_ok(c["filter"].as_str().unwrap_or("")) => {}
            Some("target") if formula_ok(&c["expr"]) => {}
            _ => out.push(
                "`compare`: {kind: \"previous_period\", filter} or {kind: \"target\", expr}".into(),
            ),
        },
    }
    if !(cfg["sparkline"].is_null() || formula_ok(&cfg["sparkline"])) {
        out.push("`sparkline`: a formula of one line".into());
    }
    if !(cfg["format"].is_null() || cfg["format"].is_object()) {
        out.push("`format`: {style, decimals}".into());
    } else if let Some(st) = cfg["format"]["style"].as_str() {
        if !matches!(st, "number" | "money" | "percent" | "compact" | "auto") {
            out.push("`format.style`: auto, number, money, percent or compact".into());
        }
    }
    out
}

pub fn filter_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    let list = match &cfg["filters"] {
        J::Null => &[][..],
        J::Array(a) if a.len() <= 20 => a.as_slice(),
        _ => return vec!["`filters`: at most 20 filters".into()],
    };
    for (k, f) in list.iter().enumerate() {
        let name = f["name"].as_str().unwrap_or("");
        let at = if super::params::name_ok(name) {
            format!("filter `{name}`")
        } else {
            format!("filter {}", k + 1)
        };
        if !super::params::name_ok(name) {
            out.push(format!("{at}: `name`: lower-case letters, digits and _ (at most 30), starting with a letter"));
        }
        if !ident_ok(&f["entity"]) {
            out.push(format!("{at}: `entity`: the entity it filters"));
        }
        if !path_ok(&f["field"]) {
            out.push(format!("{at}: `field`: a field path of at most 5 fields"));
        }
        if !f["widget"]
            .as_str()
            .is_some_and(|w| super::params::WIDGETS.contains(&w) && w != "record")
        {
            out.push(format!(
                "{at}: `widget`: select, multi, date_range, number_range, toggle or text"
            ));
        }
        if !text_ok(&f["label"], 100) {
            out.push(format!("{at}: `label`: at most 100 characters"));
        }
        if f["default"].to_string().len() > 10_000 {
            out.push(format!("{at}: `default`: too large"));
        }
    }
    for n in cfg["show"].as_array().into_iter().flatten() {
        if !super::params::name_ok(n.as_str().unwrap_or("")) {
            out.push("`show`: the names of filters defined by other filter components".into());
        }
    }
    out
}

pub fn button_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    if !text_ok(&cfg["label"], 100) {
        out.push("`label`: at most 100 characters".into());
    }
    if !(cfg["entity"].is_null() && cfg["action"].is_null()) {
        if !ident_ok(&cfg["entity"]) || !ident_ok(&cfg["action"]) {
            out.push("`entity`, `action`: the action (or the transition) it runs".into());
        }
        if !cfg["target"].is_null() {
            out.extend(from_problems("target", &cfg["target"], false));
        }
    }
    if let Some(args) = cfg["args"].as_object() {
        for (k, v) in args {
            out.extend(from_problems(&format!("args.{k}"), v, true));
        }
    } else if !cfg["args"].is_null() {
        out.push("`args`: the action's parameters → where their value comes from".into());
    }
    if !text_ok(&cfg["confirm"], 500) {
        out.push("`confirm`: at most 500 characters".into());
    }
    if !(cfg["style"].is_null()
        || matches!(
            cfg["style"].as_str(),
            Some("primary" | "secondary" | "danger")
        ))
    {
        out.push("`style`: primary, secondary or danger".into());
    }
    out
}

pub fn form_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    if !cfg["entity"].is_null() && !ident_ok(&cfg["entity"]) {
        out.push("`entity`: the entity of the records".into());
    }
    match cfg["mode"].as_str() {
        None | Some("create") => {}
        Some("edit") => {
            if !cfg["record"].is_null() {
                out.extend(from_problems("record", &cfg["record"], false));
            }
        }
        // its own fields, statements run on the server (engine/interfaces.rs)
        Some("custom") => out.extend(custom_form_problems(cfg)),
        _ => out.push("`mode`: create, edit or custom".into()),
    }
    if !(cfg["columns"].is_null()
        || cfg["columns"]
            .as_u64()
            .is_some_and(|n| (1..=3).contains(&n)))
    {
        out.push("`columns`: 1, 2 or 3".into());
    }
    match &cfg["sections"] {
        J::Null => {}
        J::Array(a) if a.len() <= 20 => {
            for s in a {
                if !text_ok(&s["title"], 200) {
                    out.push("a section's `title`: at most 200 characters".into());
                }
                let ok = s["fields"]
                    .as_array()
                    .is_some_and(|fs| fs.len() <= 100 && fs.iter().all(ident_ok));
                if !ok {
                    out.push("a section's `fields`: the names of at most 100 fields".into());
                }
            }
        }
        _ => out.push("`sections`: at most 20 sections {title, fields}".into()),
    }
    if let Some(d) = cfg["defaults"].as_object() {
        for (k, v) in d {
            out.extend(from_problems(&format!("defaults.{k}"), v, true));
        }
    } else if !cfg["defaults"].is_null() {
        out.push("`defaults`: field → where its value comes from".into());
    }
    if !text_ok(&cfg["submit_label"], 100) {
        out.push("`submit_label`: at most 100 characters".into());
    }
    out
}

/// A custom form: `fields` [{name, type, label?, where?, default?}], `submit` (statements),
/// `after` (clear, keep, open, select with `select: {name, entity}`), `message`, `confirm`.
fn custom_form_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    let mut names = HashSet::new();
    match &cfg["fields"] {
        J::Null => {}
        J::Array(a) if a.len() <= 50 => {
            for f in a {
                let n = f["name"].as_str().unwrap_or("");
                if !ident_ok(&f["name"]) || !names.insert(n.to_string()) {
                    out.push(format!(
                        "field `{n}`: a name of letters, digits and _, unique in the form"
                    ));
                }
                if !f["type"]
                    .as_str()
                    .is_some_and(|t| !t.trim().is_empty() && t.len() <= 200 && !t.contains('\n'))
                {
                    out.push(format!("field `{n}`: `type`, a type of the language (`Qty<pcs>`, `Product`, `Text?`)"));
                }
                for k in ["where", "default"] {
                    if !(f[k].is_null() || formula_ok(&f[k])) {
                        out.push(format!(
                            "field `{n}`: `{k}`: one line of at most 4000 characters"
                        ));
                    }
                }
                if !text_ok(&f["label"], 200) {
                    out.push(format!("field `{n}`: `label`: at most 200 characters"));
                }
            }
        }
        _ => out.push("`fields`: at most 50 fields {name, type}".into()),
    }
    if !(cfg["submit"].is_null() || cfg["submit"].as_str().is_some_and(|c| c.len() <= 20_000)) {
        out.push("`submit`: the statements it runs, at most 20 000 characters".into());
    }
    match cfg["after"].as_str() {
        None | Some("clear" | "keep" | "open") => {}
        Some("select") => {
            let ok = super::params::name_ok(cfg["select"]["name"].as_str().unwrap_or(""))
                && ident_ok(&cfg["select"]["entity"]);
            if !ok {
                out.push(
                    "`after: select`: `select: {name, entity}` (the record published as `@name`)"
                        .into(),
                );
            }
        }
        _ => out.push("`after`: clear, keep, open or select".into()),
    }
    if !(cfg["message"].is_null() || formula_ok(&cfg["message"])) {
        out.push("`message`: an expression of one line".into());
    }
    if !text_ok(&cfg["confirm"], 500) {
        out.push("`confirm`: at most 500 characters".into());
    }
    out
}

pub fn view_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.is_object() {
        return vec!["`config`: an object".into()];
    }
    if !cfg["entity"].is_null() && !ident_ok(&cfg["entity"]) {
        out.push("`entity`: the entity of the rows".into());
    }
    if !(cfg["view_id"].is_null() || cfg["view_id"].as_u64().is_some()) {
        out.push("`view_id`: the id of a saved view".into());
    }
    if !(cfg["type"].is_null()
        || matches!(cfg["type"].as_str(), Some("grid" | "groups" | "kanban")))
    {
        out.push("`type`: grid, groups or kanban".into());
    }
    if !(cfg["view"].is_null() || cfg["view"].is_object()) {
        out.push("`view`: a view's configuration".into());
    } else if let Some(w) = cfg["view"]["where"].as_str() {
        if w.contains('\n') || w.len() > 4000 {
            out.push("`view.where`: one line of at most 4000 characters".into());
        }
    }
    if !(cfg["select"].is_null() || super::params::name_ok(cfg["select"].as_str().unwrap_or(""))) {
        out.push("`select`: the name its selected record is known by (`order`: `@order`)".into());
    }
    if !(cfg["page_size"].is_null()
        || cfg["page_size"]
            .as_u64()
            .is_some_and(|n| (1..=500).contains(&n)))
    {
        out.push("`page_size`: 1 to 500".into());
    }
    // what its users may change through it (nothing by default); the API checks each write
    // against the user's permissions whatever this says
    match &cfg["edit"] {
        J::Null => {}
        J::Object(o) => {
            for (k, v) in o {
                match k.as_str() {
                    "cells" | "add" | "move" if v.is_boolean() => {}
                    "cells" | "add" | "move" => out.push(format!("`edit.{k}`: true or false")),
                    "delete" => out.push("`edit.delete`: records are not deleted in Quadra (cancel or close them with an action: a button)".into()),
                    _ => out.push(format!("`edit.{k}`: unknown (cells, add, move)")),
                }
            }
            if o.get("move") == Some(&J::Bool(true))
                && cfg["view_id"].is_null()
                && matches!(cfg["type"].as_str(), Some("grid" | "groups"))
            {
                out.push("`edit.move`: only a board moves its cards".into());
            }
            if o.get("add") == Some(&J::Bool(true))
                && cfg["view_id"].is_null()
                && matches!(cfg["type"].as_str(), Some("groups" | "kanban"))
            {
                out.push("`edit.add`: only a grid adds rows".into());
            }
        }
        _ => out.push(
            "`edit`: what may be changed through it ({cells, add, move}: true or false)".into(),
        ),
    }
    out
}

/// A spreadsheet component: its sheets, cells, styles, sizes.
pub fn workbook_problems(cfg: &J) -> Vec<String> {
    let mut out = Vec::new();
    let Some(sheets) = cfg["sheets"].as_array() else {
        return vec!["`sheets`: a list of sheets".into()];
    };
    if sheets.is_empty() || sheets.len() > 50 {
        out.push("1 to 50 sheets".into());
    }
    let mut names: Vec<String> = Vec::new();
    let mut ids = HashSet::new();
    for (k, s) in sheets.iter().enumerate() {
        let name = s["name"].as_str().unwrap_or("");
        let at = format!(
            "sheet {}",
            if name.is_empty() {
                (k + 1).to_string()
            } else {
                format!("`{name}`")
            }
        );
        if !id_ok(s["id"].as_str().unwrap_or(""))
            || !ids.insert(s["id"].as_str().unwrap_or("").to_string())
        {
            out.push(format!(
                "{at}: `id` must be unique, letters, digits, - and _"
            ));
        }
        if name.trim().is_empty()
            || name.chars().count() > 100
            || name.contains(['!', '\'', '[', ']', '\n'])
        {
            out.push(format!(
                "{at}: a name of 1 to 100 characters, without ! ' [ ]"
            ));
        }
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            out.push(format!("{at}: two sheets with this name"));
        }
        names.push(name.to_string());
        match s["mode"].as_str() {
            None | Some("fixed" | "free") => {}
            _ => out.push(format!("{at}: `mode`: fixed or free")),
        }
        let rows = s["rows"].as_u64().unwrap_or(1);
        let cols = s["cols"].as_u64().unwrap_or(1);
        if rows == 0 || rows > MAX_ROWS as u64 || cols == 0 || cols > MAX_COLS as u64 {
            out.push(format!(
                "{at}: 1 to {MAX_ROWS} rows, 1 to {MAX_COLS} columns"
            ));
        }
        match &s["cells"] {
            J::Null => {}
            J::Object(cells) => {
                for (a, c) in cells {
                    if parse_a1(a).is_none() {
                        out.push(format!("{at}: `{a}` is not a cell"));
                        continue;
                    }
                    let v = match c {
                        J::String(t) => Some(t.as_str()),
                        J::Object(_) => c["v"].as_str(),
                        _ => {
                            out.push(format!("{at}, {a}: a cell is {{v, s}}"));
                            continue;
                        }
                    };
                    if let Some(v) = v {
                        if v.len() > 32_000
                            || (v.starts_with('=') && (v.len() > 4001 || v.contains('\n')))
                        {
                            out.push(format!("{at}, {a}: a formula is one line of at most 4000 characters; a value at most 32 000"));
                        }
                    }
                    if !(c["s"].is_null() || c["s"].is_object()) {
                        out.push(format!("{at}, {a}: `s` (the style) is an object"));
                    }
                }
            }
            _ => out.push(format!("{at}: `cells`: an object by address (A1)")),
        }
        for m in s["merges"].as_array().into_iter().flatten() {
            let ok = m
                .as_str()
                .and_then(|m| m.split_once(':'))
                .is_some_and(|(a, b)| parse_a1(a).is_some() && parse_a1(b).is_some());
            if !ok {
                out.push(format!("{at}: a merge is a range like A1:C2"));
            }
        }
        if out.len() > 50 {
            break;
        }
    }
    out
}

impl Engine {
    /// `GET /v1/apps/{id}/export.xlsx?component=…&scope=…`: a component of an app as an Excel
    /// file, evaluated as the caller with the app's filters as they are set (`scope`: the
    /// `params` and `bind` of `/v1/eval`): a spreadsheet with its styles, a chart's series or
    /// a KPI's figures as a table.
    pub(super) fn app_export(
        &mut self,
        p: &Principal,
        who: &super::views::Who,
        id: u64,
        query: &HashMap<String, String>,
    ) -> Result<Reply, Fail> {
        let (app, _, _) = self.visible(who, id)?;
        if app.kind != "app" {
            return Err((404, format!("app {id} not found")));
        }
        let scope: J = match query.get("scope") {
            Some(t) if !t.is_empty() => {
                serde_json::from_str(t).map_err(|e| (400, format!("`scope`: not JSON: {e}")))?
            }
            _ => J::Null,
        };
        let comp_id = query.get("component").cloned().unwrap_or_default();
        let all: Vec<J> = app.config["pages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p["components"].as_array().cloned().unwrap_or_default())
            .collect();
        let exportable = |c: &J| matches!(c["type"].as_str(), Some("sheet" | "chart" | "kpi"));
        let comp = all
            .iter()
            .find(|c| {
                if comp_id.is_empty() {
                    c["type"] == json!("sheet")
                } else {
                    c["id"] == json!(comp_id) && exportable(c)
                }
            })
            .cloned()
            .ok_or((
                404,
                format!("no spreadsheet, chart or KPI `{comp_id}` in app {id}"),
            ))?;
        let title = comp["title"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&app.name)
            .to_string();
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let bytes = if comp["type"] == json!("sheet") {
            let cfg = comp["config"].clone();
            let book = sheets::Book::with_scope(&cfg, &scope)?;
            let ((outs, _, _, _), _, _, spills) = self.eval_book(p, &book)?;
            let grid = sheets::grid_of(&book, &outs, &spills);
            let mut i = self.interp(model, Some(p), None)?;
            xlsx::workbook(&cfg, &grid, &mut i)
                .map_err(|e| (500, format!("the Excel file: {e}")))?
        } else {
            let rows = self.component_rows(p, &comp, &all, &scope)?;
            let mut i = self.interp(model, Some(p), None)?;
            table_xlsx(&title, &rows, &mut i)?
        };
        let file: String = title
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Ok(file_reply(
            &format!("{}.xlsx", file.trim()),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            &bytes,
        ))
    }

    /// One expression of a component, as the caller with the scope: its value.
    fn eval_expr(&mut self, p: &Principal, expr: &str, scope: &J) -> Result<sheets::Out, Fail> {
        let book =
            sheets::Book::with_scope(&json!({ "sheets": [], "exprs": { "x": expr } }), scope)?;
        let ((outs, _, _, _), _, _, _) = self.eval_book(p, &book)?;
        Ok(outs.into_values().next().unwrap_or(sheets::Out::Err {
            code: "#ERROR",
            msg: "no value".into(),
        }))
    }

    /// The rectangle of a spreadsheet component of the app, as it shows (values typed,
    /// results, spills): rows of cells.
    fn sheet_range(
        &mut self,
        p: &Principal,
        all: &[J],
        component: &str,
        range: &str,
        scope: &J,
    ) -> Result<Vec<Vec<sheets::Out>>, Fail> {
        let sheet = all
            .iter()
            .find(|c| c["id"] == json!(component) && c["type"] == json!("sheet"))
            .ok_or((404, format!("no spreadsheet `{component}` in the app")))?;
        let cfg = sheet["config"].clone();
        // its own filters are those its bindings say (no automatic ones)
        let sc = json!({ "params": scope["params"], "bind": sheet_bind(&sheet["bindings"], &scope["params"]) });
        let book = sheets::Book::with_scope(&cfg, &sc)?;
        let (sname, rect) = match range.rsplit_once('!') {
            Some((n, r)) => (Some(n.trim_matches('\'').to_string()), r),
            None => (None, range),
        };
        let s = match &sname {
            Some(n) => book
                .names
                .iter()
                .position(|x| x.eq_ignore_ascii_case(n))
                .ok_or((400, format!("no sheet named `{n}`")))?,
            None => 0,
        };
        let (a, b) = rect.split_once(':').unwrap_or((rect, rect));
        let clean = |x: &str| x.replace('$', "");
        let ((r1, c1), (r2, c2)) = (
            sheets::parse_a1(&clean(a)).ok_or((400, format!("`{range}` is not a range")))?,
            sheets::parse_a1(&clean(b)).ok_or((400, format!("`{range}` is not a range")))?,
        );
        let (r1, r2, c1, c2) = (r1.min(r2), r1.max(r2), c1.min(c2), c1.max(c2));
        if (r2 - r1 + 1) as u64 * (c2 - c1 + 1) as u64 > 200_000 {
            return Err((400, "at most 200 000 cells".into()));
        }
        let ((outs, _, _, _), _, _, spills) = self.eval_book(p, &book)?;
        let grid = sheets::grid_of(&book, &outs, &spills);
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(p), None)?;
        let mut rows = Vec::new();
        for r in r1..=r2 {
            let mut row = Vec::new();
            for c in c1..=c2 {
                let o = grid
                    .get(s)
                    .and_then(|g| g.get(&(r, c)).cloned())
                    .or_else(|| {
                        let raw = book.cells.get(s)?.get(&(r, c))?;
                        match sheets::classify(&mut i, raw)? {
                            sheets::Input::Value(v) => Some(sheets::Out::Ok(v)),
                            sheets::Input::Formula(_) => None,
                        }
                    });
                row.push(o.unwrap_or(sheets::Out::Ok(V::None)));
            }
            rows.push(row);
        }
        // trailing empty rows are not data
        while rows
            .last()
            .is_some_and(|r| r.iter().all(|o| matches!(o, sheets::Out::Ok(V::None))))
        {
            rows.pop();
        }
        Ok(rows)
    }

    /// What a chart or a KPI shows, as a table (the first row: the names of the columns).
    fn component_rows(
        &mut self,
        p: &Principal,
        comp: &J,
        all: &[J],
        scope: &J,
    ) -> Result<Vec<Vec<sheets::Out>>, Fail> {
        let cfg = &comp["config"];
        let src = &cfg["source"];
        let text = |t: &str| sheets::Out::Ok(V::Text(Rc::from(t)));
        let mut out: Vec<Vec<sheets::Out>> = Vec::new();
        if comp["type"] == json!("chart") {
            if src["type"] == json!("range") {
                return self.sheet_range(
                    p,
                    all,
                    src["component"].as_str().unwrap_or(""),
                    src["range"].as_str().unwrap_or(""),
                    scope,
                );
            }
            let v = match self.eval_expr(p, src["expr"].as_str().unwrap_or(""), scope)? {
                sheets::Out::Ok(v) => v,
                sheets::Out::Err { code, msg } => return Err((422, format!("{code}: {msg}"))),
            };
            match sheets::spill_of(&v) {
                Some(sp) => {
                    let header: Vec<String> = sp.header.clone().unwrap_or_else(|| {
                        (0..sp.width)
                            .map(|k| {
                                if k == 0 {
                                    "Label".to_string()
                                } else if sp.width == 2 {
                                    "Value".to_string()
                                } else {
                                    format!("Series {k}")
                                }
                            })
                            .collect()
                    });
                    out.push(header.iter().map(|h| text(h)).collect());
                    let max = cfg["max_points"].as_u64().unwrap_or(500).clamp(1, 5000) as usize;
                    for r in sp.rows.into_iter().take(max) {
                        out.push(r.into_iter().map(sheets::Out::Ok).collect());
                    }
                }
                None => {
                    out.push(vec![text("Value")]);
                    out.push(vec![sheets::Out::Ok(v)]);
                }
            }
            return Ok(out);
        }
        // a KPI: its value, and what it is compared with
        let label = cfg["label"]
            .as_str()
            .filter(|l| !l.is_empty())
            .unwrap_or("Value")
            .to_string();
        let value = |me: &mut Engine, scope: &J| -> Result<sheets::Out, Fail> {
            if src["type"] == json!("cell") {
                let cell = src["cell"].as_str().unwrap_or("");
                let r =
                    me.sheet_range(p, all, src["component"].as_str().unwrap_or(""), cell, scope)?;
                Ok(r.into_iter()
                    .next()
                    .and_then(|row| row.into_iter().next())
                    .unwrap_or(sheets::Out::Ok(V::None)))
            } else {
                me.eval_expr(p, src["expr"].as_str().unwrap_or(""), scope)
            }
        };
        out.push(vec![text(""), text(&label)]);
        out.push(vec![text("Value"), value(self, scope)?]);
        match cfg["compare"]["kind"].as_str() {
            Some("target") => {
                let t = self.eval_expr(p, cfg["compare"]["expr"].as_str().unwrap_or(""), scope)?;
                out.push(vec![text("Target"), t]);
            }
            Some("previous_period") => {
                let name = cfg["compare"]["filter"].as_str().unwrap_or("");
                let mut sc = scope.clone();
                let mut shifted = false;
                for prm in sc["params"].as_array_mut().into_iter().flatten() {
                    if prm["name"] == json!(name) {
                        if let Some(r) = previous_range(&prm["value"]) {
                            prm["value"] = r;
                            shifted = true;
                        }
                    }
                }
                if shifted {
                    let prev = value(self, &sc)?;
                    out.push(vec![text("Previous period"), prev]);
                }
            }
            _ => {}
        }
        if let Some(e) = cfg["sparkline"].as_str().filter(|e| !e.trim().is_empty()) {
            if let sheets::Out::Ok(v) = self.eval_expr(p, e, scope)? {
                if let Some(sp) = sheets::spill_of(&v) {
                    out.push(Vec::new());
                    out.push(vec![text("Trend")]);
                    for r in sp.rows {
                        out.push(r.into_iter().map(sheets::Out::Ok).collect());
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The filters a spreadsheet's formulas get on their own: those its bindings bind.
fn sheet_bind(bindings: &J, params: &J) -> J {
    let known: Vec<&str> = params
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["name"].as_str())
        .collect();
    let mut out = serde_json::Map::new();
    for (e, m) in bindings["filters"].as_object().into_iter().flatten() {
        let Some(m) = m.as_object() else { continue };
        let kept: serde_json::Map<String, J> = m
            .iter()
            .filter(|(k, v)| v.is_string() && known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !kept.is_empty() {
            out.insert(e.clone(), J::Object(kept));
        }
    }
    J::Object(out)
}

/// The period of the same length just before a date range (`[from, to]`): whole months move
/// by months, otherwise by days (as the KPI of the web client compares).
pub fn previous_range(v: &J) -> Option<J> {
    use chrono::{Datelike, NaiveDate};
    let d = |j: &J| {
        j.as_str()
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
    };
    let (a, b) = (d(&v[0])?, d(&v[1])?);
    let month_start = a.day() == 1;
    let month_end = b.succ_opt()?.day() == 1;
    let (pa, pb) = if month_start && month_end {
        let months = (b.year() - a.year()) * 12 + b.month() as i32 - a.month() as i32 + 1;
        let start = a.checked_sub_months(chrono::Months::new(months as u32))?;
        (start, a.pred_opt()?)
    } else {
        let span = (b - a).num_days() + 1;
        (a - chrono::Duration::days(span), a.pred_opt()?)
    };
    Some(json!([pa.to_string(), pb.to_string()]))
}

/// A table as an Excel file: one sheet, its first row in bold.
fn table_xlsx(title: &str, rows: &[Vec<sheets::Out>], i: &mut Interp) -> Result<Vec<u8>, Fail> {
    let mut cells = serde_json::Map::new();
    let mut grid: BTreeMap<(u32, u32), sheets::Out> = BTreeMap::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, o) in row.iter().enumerate() {
            let at = ((r + 1) as u32, (c + 1) as u32);
            if r == 0 {
                cells.insert(sheets::a1(at.0, at.1), json!({ "s": { "b": true } }));
            }
            grid.insert(at, o.clone());
        }
    }
    let name: String = title
        .chars()
        .filter(|c| !"[]:*?/\\'".contains(*c))
        .take(31)
        .collect();
    let cfg = json!({ "sheets": [{ "id": "data", "name": if name.trim().is_empty() { "Data".to_string() } else { name }, "cells": cells, "freeze": { "rows": 1 } }] });
    xlsx::workbook(&cfg, &[grid], i).map_err(|e| (500, format!("the Excel file: {e}")))
}

/// A file as a reply: the HTTP layer sends the bytes (`__file`), not JSON.
pub fn file_reply(name: &str, content_type: &str, bytes: &[u8]) -> Reply {
    reply(
        200,
        json!({ "__file": { "name": name, "type": content_type, "hex": hex(bytes) } }),
    )
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    };
    b.chunks(2)
        .map(|p| Some(v(p[0])? << 4 | v(p[1])?))
        .collect()
}

// ---- what the web client asks to show an app ---------------------------------------------------

/// A request the web client makes to show a component of an app.
#[derive(Clone, Debug)]
pub enum AppRequest {
    /// `POST /v1/eval`
    Eval { component: String, body: J },
    /// `POST /v1/query` (`group_by` included); `columns`: a board's counts, whose groups each
    /// get a query of their first cards (`column` with the group's filter)
    Query {
        component: String,
        body: J,
        columns: Option<J>,
    },
    /// `GET /v1/records/{entity}/{id}` (a button's or a form's record)
    Record {
        component: String,
        entity: String,
        id: u64,
    },
    /// `GET /v1/model`
    Model,
}

/// The app's parameters as the client sends them (web/src/apps/params.ts: `paramDefs`,
/// `evalParams`): the filters of its filter components, then the selections of its views,
/// with their values (`values`: by name; absent: not set).
pub fn app_params(cfg: &J, values: &J) -> Vec<J> {
    let mut out: Vec<J> = Vec::new();
    let comps: Vec<&J> = cfg["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["components"].as_array().into_iter().flatten())
        .collect();
    for c in &comps {
        if c["type"] == json!("filter") {
            for f in c["config"]["filters"].as_array().into_iter().flatten() {
                let name = f["name"].as_str().unwrap_or("");
                if !super::params::name_ok(name) || out.iter().any(|p| p["name"] == json!(name)) {
                    continue;
                }
                let widget = f["widget"].as_str().unwrap_or("select");
                let v = values.get(name).cloned().unwrap_or(J::Null);
                let v = match (widget, v) {
                    ("multi", J::Null) => json!([]),
                    ("toggle", J::Null) => json!(false),
                    ("text", J::Null) => json!(""),
                    (_, v) => v,
                };
                out.push(json!({ "name": name, "entity": f["entity"], "field": f["field"], "widget": widget, "value": v }));
            }
        }
    }
    for c in &comps {
        // the record a view selects, the one a custom form published (`after: select`)
        let sel = match c["type"].as_str() {
            Some("view") => c["config"]["select"]
                .as_str()
                .zip(c["config"]["entity"].as_str()),
            Some("form") if c["config"]["after"] == json!("select") => c["config"]["select"]
                ["name"]
                .as_str()
                .zip(c["config"]["select"]["entity"].as_str()),
            _ => None,
        };
        if let Some((name, e)) = sel {
            if super::params::name_ok(name) && !out.iter().any(|p| p["name"] == json!(name)) {
                out.push(json!({ "name": name, "entity": e, "widget": "record", "value": values.get(name).cloned().unwrap_or(J::Null) }));
            }
        }
    }
    out
}

/// The entities a component's filters apply to (params.ts `bindFor`).
pub fn bind_for(params: &[J], bindings: &J, auto: bool) -> J {
    let mut out = serde_json::Map::new();
    let b = &bindings["filters"];
    if auto && b["auto"] != json!(false) {
        for p in params {
            if p["widget"] == json!("record") {
                continue;
            }
            if let (Some(e), Some(f)) = (p["entity"].as_str(), p["field"].as_str()) {
                out.entry(e.to_string()).or_insert_with(|| json!({}))
                    [p["name"].as_str().unwrap_or("")] = json!(f);
            }
        }
    }
    for (e, m) in b.as_object().into_iter().flatten() {
        if e == "auto" {
            continue;
        }
        for (name, path) in m.as_object().into_iter().flatten() {
            if !params.iter().any(|p| p["name"] == json!(name)) {
                continue;
            }
            match path.as_str().filter(|p| !p.is_empty()) {
                Some(path) => {
                    out.entry(e.clone()).or_insert_with(|| json!({}))[name.as_str()] = json!(path);
                }
                None => {
                    if let Some(o) = out.get_mut(e).and_then(|x| x.as_object_mut()) {
                        o.remove(name);
                    }
                }
            }
        }
    }
    out.retain(|_, v| v.as_object().is_some_and(|o| !o.is_empty()));
    J::Object(out)
}

/// The conditions of `/v1/query` for the rows of `entity` (params.ts `queryFilters`).
pub fn query_filters(entity: &str, params: &[J], bind: &J) -> Vec<J> {
    let mut out = Vec::new();
    for (name, path) in bind[entity].as_object().into_iter().flatten() {
        let Some(p) = params.iter().find(|p| p["name"] == json!(name)) else {
            continue;
        };
        let v = &p["value"];
        let unset = match v {
            J::Null => true,
            J::Array(a) => a.is_empty() || a.iter().all(|x| x.is_null()),
            J::Bool(b) => !b,
            J::String(s) => s.trim().is_empty(),
            _ => false,
        };
        if unset {
            continue;
        }
        let op = match p["widget"].as_str().unwrap_or("") {
            "multi" => "in",
            "date_range" | "number_range" => "range",
            "toggle" => "true",
            "text" => "contains",
            _ => "eq",
        };
        out.push(json!({ "path": path, "op": op, "value": v }));
    }
    out
}

/// The expressions and the formulas of a request with an app's `let`s in place.
pub fn with_lets(lets: &[(String, String)], exprs: J, sheets: J) -> (J, J) {
    if lets.is_empty() {
        return (exprs, sheets);
    }
    let expand = |t: &str| super::interfaces::expand_lets(t, lets);
    let exprs = match exprs {
        J::Object(o) => J::Object(
            o.into_iter()
                .map(|(k, v)| (k, v.as_str().map(|t| json!(expand(t))).unwrap_or(v)))
                .collect(),
        ),
        other => other,
    };
    let sheets = match sheets {
        J::Array(a) => J::Array(
            a.into_iter()
                .map(|mut s| {
                    if let Some(cells) = s["cells"].as_object_mut() {
                        for c in cells.values_mut() {
                            if let Some(v) = c["v"]
                                .as_str()
                                .filter(|v| v.starts_with('='))
                                .map(|v| format!("={}", expand(&v[1..])))
                            {
                                c["v"] = json!(v);
                            }
                        }
                    }
                    s
                })
                .collect(),
        ),
        other => other,
    };
    (exprs, sheets)
}

/// The expressions of a markdown text: `{= … }`.
fn text_exprs(t: &str) -> serde_json::Map<String, J> {
    let mut out = serde_json::Map::new();
    let mut rest = t;
    while let Some(k) = rest.find("{=") {
        let after = &rest[k + 2..];
        let Some(end) = after.find('}') else { break };
        out.insert(format!("e{}", out.len()), json!(after[..end].trim()));
        rest = &after[end + 1..];
    }
    out
}

/// What the web client asks, component by component, to show the pages of an app with the
/// parameters' `values` (the requests of its first evaluation: the benchmark's `app_open`).
pub fn open_requests(cfg: &J, values: &J) -> Vec<AppRequest> {
    let params = app_params(cfg, values);
    let lets = super::interfaces::lets_of(cfg);
    let mut out = vec![AppRequest::Model];
    let scope = |bindings: &J, auto: bool| json!({ "params": params, "bind": bind_for(&params, bindings, auto) });
    for c in cfg["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["components"].as_array().cloned().unwrap_or_default())
    {
        let id = c["id"].as_str().unwrap_or("").to_string();
        let k = &c["config"];
        let mut eval = |exprs: J, sheets: J, sc: &J| {
            // the app's `let`s, as the client sends them: in place
            let (exprs, sheets) = with_lets(&lets, exprs, sheets);
            let mut body = json!({ "sheets": sheets, "exprs": exprs });
            if !params.is_empty() {
                body["params"] = sc["params"].clone();
                body["bind"] = sc["bind"].clone();
            }
            out.push(AppRequest::Eval {
                component: id.clone(),
                body,
            });
        };
        match c["type"].as_str().unwrap_or("") {
            "markdown" => {
                let ex = text_exprs(k["text"].as_str().unwrap_or(""));
                if !ex.is_empty() {
                    eval(J::Object(ex), json!([]), &scope(&c["bindings"], false));
                }
            }
            "sheet" => {
                let sheets: Vec<J> = k["sheets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        let cells: serde_json::Map<String, J> = s["cells"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .filter_map(|(a, x)| {
                                x["v"]
                                    .as_str()
                                    .or(x.as_str())
                                    .filter(|v| !v.is_empty())
                                    .map(|v| (a.clone(), json!({ "v": v })))
                            })
                            .collect();
                        json!({ "id": s["id"], "name": s["name"], "cells": cells })
                    })
                    .collect();
                eval(json!({}), json!(sheets), &scope(&c["bindings"], false));
            }
            "chart" | "kpi" => {
                let sc = scope(&c["bindings"], true);
                let mut ex = serde_json::Map::new();
                if k["source"]["type"] == json!("expr") {
                    ex.insert(
                        if c["type"] == json!("chart") {
                            "data".into()
                        } else {
                            "value".into()
                        },
                        k["source"]["expr"].clone(),
                    );
                }
                if let Some(t) = k["compare"]["expr"]
                    .as_str()
                    .filter(|_| k["compare"]["kind"] == json!("target"))
                {
                    ex.insert("target".into(), json!(t));
                }
                if let Some(sp) = k["sparkline"].as_str().filter(|s| !s.trim().is_empty()) {
                    ex.insert("spark".into(), json!(sp));
                }
                if !ex.is_empty() {
                    eval(J::Object(ex), json!([]), &sc);
                }
                // the previous period: the same value, the range moved back
                if k["compare"]["kind"] == json!("previous_period")
                    && k["source"]["type"] == json!("expr")
                {
                    let name = k["compare"]["filter"].as_str().unwrap_or("");
                    let mut sc2 = sc.clone();
                    let mut moved = false;
                    for p in sc2["params"].as_array_mut().into_iter().flatten() {
                        if p["name"] == json!(name) {
                            if let Some(r) = previous_range(&p["value"]) {
                                p["value"] = r;
                                moved = true;
                            }
                        }
                    }
                    if moved {
                        eval(json!({ "value": k["source"]["expr"] }), json!([]), &sc2);
                    }
                }
            }
            "view" => {
                let Some(entity) = k["entity"].as_str() else {
                    continue;
                };
                let sc = scope(&c["bindings"], true);
                let filters = query_filters(entity, &params, &sc["bind"]);
                let view = &k["view"];
                let page = k["page_size"].as_u64().unwrap_or(50);
                let fields = view["fields"].clone();
                let mut base = json!({ "entity": entity, "filters": filters });
                if let Some(w) = view["where"].as_str().filter(|w| !w.is_empty()) {
                    base["where"] = json!(w);
                }
                match k["type"].as_str().unwrap_or("grid") {
                    "kanban" => {
                        let field = view["kanban"]["field"].as_str().unwrap_or("state");
                        let mut counts = base.clone();
                        counts["group_by"] = json!([field]);
                        counts["limit"] = json!(100);
                        if let Some(a) = view["kanban"]["aggregate"].as_str() {
                            counts["aggregates"] = json!({ a: "sum" });
                        }
                        let mut column = base.clone();
                        let mut cards: Vec<J> = view["kanban"]["cards"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        if let Some(t) = view["kanban"]["title"].as_str() {
                            cards.push(json!(t));
                        }
                        column["fields"] = json!(cards);
                        column["limit"] = json!(page);
                        column["count"] = json!(false);
                        column["__group_field"] = json!(field);
                        out.push(AppRequest::Query {
                            component: id.clone(),
                            body: counts,
                            columns: Some(column),
                        });
                    }
                    "groups" => {
                        let mut g = base.clone();
                        g["group_by"] = view["group"]["keys"].clone();
                        g["limit"] = json!(100);
                        out.push(AppRequest::Query {
                            component: id.clone(),
                            body: g,
                            columns: None,
                        });
                    }
                    _ => {
                        let mut q = base.clone();
                        q["fields"] = fields;
                        q["limit"] = json!(page);
                        q["count"] = json!(true);
                        out.push(AppRequest::Query {
                            component: id.clone(),
                            body: q,
                            columns: None,
                        });
                    }
                }
            }
            "button" | "form" => {
                let src = if c["type"] == json!("button") {
                    &k["target"]
                } else {
                    &k["record"]
                };
                let entity = k["entity"].as_str().unwrap_or("").to_string();
                let rid = match src["from"].as_str() {
                    Some("param") => values
                        .get(src["name"].as_str().unwrap_or(""))
                        .and_then(|v| {
                            v.as_u64().or_else(|| {
                                v.as_array()
                                    .and_then(|a| a.first())
                                    .and_then(|x| x.as_u64())
                            })
                        }),
                    Some("id") => src["id"].as_u64(),
                    _ => None,
                };
                if let Some(rid) =
                    rid.filter(|_| c["type"] == json!("button") || k["mode"] == json!("edit"))
                {
                    out.push(AppRequest::Record {
                        component: id.clone(),
                        entity,
                        id: rid,
                    });
                }
            }
            _ => {}
        }
    }
    out
}
