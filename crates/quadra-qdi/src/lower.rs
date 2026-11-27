//! A tree → the JSON definitions the server keeps: `{kind, entity, type, name, description,
//! config, shares}` for each app and saved view of the file (the objects of the export of
//! `quadra db views export`, without their owner).
//!
//! What a file leaves out is the default the client and the server use when a key is missing
//! (options.rs `is_default`), so it is left out of the JSON too; the structures the client
//! keeps whole (a board's `kanban`, a view's `group`, a sheet's `cells`, a component's
//! `bindings`) are written whole.

use crate::ast::*;
use crate::layout;
use crate::options;
use crate::Diag;
use serde_json::{json, Map, Value as J};
use std::collections::{BTreeMap, HashMap, HashSet};

pub fn lower(file: &File) -> Result<Vec<J>, Vec<Diag>> {
    let mut diags = Vec::new();
    let mut out = Vec::new();
    let mut apps = 0;
    let mut view_names: HashSet<String> = HashSet::new();
    for item in &file.items {
        match item {
            Item::App(a) => {
                apps += 1;
                if apps > 1 {
                    diags.push(Diag::new(
                        "a file holds one app (its path is the app's)",
                        a.span,
                    ));
                }
                out.push(lower_app(a, &mut diags));
            }
            Item::View(v) => {
                if !view_names.insert(v.name.clone()) {
                    diags.push(Diag::new(
                        format!("two views named \"{}\" in the file", v.name),
                        v.span,
                    ));
                }
                out.push(lower_saved_view(v, &mut diags));
            }
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        diags.sort_by_key(|d| d.span.start);
        Err(diags)
    }
}

/// Sets `path` in `obj`, making the objects on the way.
pub fn set(obj: &mut J, path: &[&str], v: J) {
    if path.is_empty() {
        *obj = v;
        return;
    }
    if !obj.is_object() {
        *obj = J::Object(Map::new());
    }
    let m = obj.as_object_mut().unwrap();
    if path.len() == 1 {
        m.insert(path[0].to_string(), v);
        return;
    }
    let next = m
        .entry(path[0].to_string())
        .or_insert_with(|| J::Object(Map::new()));
    set(next, &path[1..], v);
}

fn set_dotted(obj: &mut J, path: &str, v: J) {
    let segs: Vec<&str> = path.split('.').collect();
    set(obj, &segs, v);
}

fn apply_raws(obj: &mut J, raws: &[&Raw]) {
    for r in raws {
        let p: Vec<&str> = r.path.iter().map(String::as_str).collect();
        set(obj, &p, r.value.clone());
    }
}

pub fn source_json(s: &Source) -> J {
    match s {
        Source::Param(n) => json!({ "from": "param", "name": n }),
        Source::Id(n) => json!({ "from": "id", "id": n }),
        Source::Cell { component, cell } => {
            json!({ "from": "cell", "component": component, "cell": cell })
        }
        Source::Value(v) => json!({ "from": "value", "value": v }),
    }
}

fn shares_json(s: &[Share]) -> J {
    J::Array(
        s.iter()
            .map(|s| match &s.role {
                Some(r) => json!({ "type": "role", "name": r, "edit": s.change }),
                None => json!({ "type": "everyone", "edit": s.change }),
            })
            .collect(),
    )
}

/// The first `prefix_N` not taken.
fn fresh(prefix: &str, taken: &mut HashSet<String>) -> String {
    let mut n = 1;
    loop {
        let id = format!("{prefix}_{n}");
        if taken.insert(id.clone()) {
            return id;
        }
        n += 1;
    }
}

fn meta_id(raws: &[Raw]) -> Option<String> {
    raws.iter()
        .find(|r| r.meta && r.path == ["id"])
        .and_then(|r| r.value.as_str().map(str::to_string))
}

fn line_raws(lines: &[Line]) -> Vec<Raw> {
    lines
        .iter()
        .filter_map(|l| {
            if let Line::Raw(r) = l {
                Some(r.clone())
            } else {
                None
            }
        })
        .collect()
}

// ---- apps -----------------------------------------------------------------------------------------

fn lower_app(a: &App, diags: &mut Vec<Diag>) -> J {
    let mut config = json!({ "version": 1 });
    if let Some(c) = &a.comment {
        config["comment"] = json!(c);
    }
    // the selections the app publishes (`select: order` of a view of Order, `after: select`)
    let mut selections: HashMap<String, String> = HashMap::new();
    let mut comp_ids: HashSet<String> = HashSet::new();
    for p in &a.pages {
        visit(&p.body, &mut |c| {
            if let Some(n) = &c.name {
                if !comp_ids.insert(n.text.clone()) {
                    diags.push(Diag::new(
                        format!("two components named `{}` in the app", n.text),
                        n.span,
                    ));
                }
            } else if let Some(id) = meta_id(&line_raws(&c.lines)) {
                comp_ids.insert(id);
            }
            if c.kind == "view" {
                let opt = |k: &str| {
                    c.lines.iter().find_map(|l| match l {
                        Line::Opt {
                            key,
                            value: Value::Word(w),
                            ..
                        } if key.text == k => Some(w.clone()),
                        _ => None,
                    })
                };
                if let (Some(s), Some(e)) = (opt("select"), opt("of")) {
                    selections.entry(s).or_insert(e);
                }
            }
            if c.kind == "form" {
                for l in &c.lines {
                    if let Line::Opt {
                        value: Value::After(After::Select { entity, name }),
                        ..
                    } = l
                    {
                        selections.entry(name.clone()).or_insert(entity.clone());
                    }
                }
            }
        });
    }
    // the filters
    let mut defs: BTreeMap<String, (J, Span)> = BTreeMap::new();
    for f in &a.filters {
        let mut d = json!({ "name": f.name.text, "entity": f.entity.text, "field": f.field.text, "widget": f.widget.text });
        if let Some(l) = &f.label {
            d["label"] = json!(l);
        }
        if let Some(v) = &f.default {
            d["default"] = v.clone();
        }
        if let Some(c) = &f.comment {
            d["comment"] = json!(c);
        }
        apply_raws(&mut d, &f.raws.iter().collect::<Vec<_>>());
        if defs.insert(f.name.text.clone(), (d, f.name.span)).is_some() {
            diags.push(Diag::new(
                format!("two filters named `{}`", f.name.text),
                f.name.span,
            ));
        }
        if selections.contains_key(&f.name.text) {
            diags.push(Diag::new(
                format!("`@{}` is both a filter and a selection", f.name.text),
                f.name.span,
            ));
        }
    }
    let mut owned: HashSet<String> = HashSet::new();
    let mut page_ids: HashSet<String> = a
        .pages
        .iter()
        .filter_map(|p| p.id.as_ref().map(|n| n.text.clone()))
        .collect();
    let mut pages = Vec::new();
    let mut tab_ids: HashSet<String> = HashSet::new();
    for p in &a.pages {
        let id =
            p.id.as_ref()
                .map(|n| n.text.clone())
                .or_else(|| meta_id(&p.raws))
                .unwrap_or_else(|| fresh("page", &mut page_ids));
        let mut placed = layout::place_page(&p.body, diags);
        placed.sort_by_key(|(_, r)| (r[1], r[0]));
        let mut comps = Vec::new();
        for (c, r) in placed {
            let id = c
                .name
                .as_ref()
                .map(|n| n.text.clone())
                .or_else(|| meta_id(&line_raws(&c.lines)))
                .unwrap_or_else(|| fresh(&c.kind, &mut comp_ids));
            let (mut j, show) = lower_component(c, &id, r, &selections, &mut tab_ids, diags);
            if let Some(show) = show {
                let mut own = Vec::new();
                let mut borrowed = Vec::new();
                for n in show {
                    match defs.get(&n) {
                        Some((d, _)) if owned.insert(n.clone()) => own.push(d.clone()),
                        _ => borrowed.push(json!(n)),
                    }
                }
                if !own.is_empty() {
                    j["config"]["filters"] = J::Array(own);
                }
                if !borrowed.is_empty() {
                    j["config"]["show"] = J::Array(borrowed);
                }
            }
            comps.push(j);
        }
        let mut pj = json!({ "id": id, "components": comps });
        if let Some(n) = &p.name {
            pj["name"] = json!(n);
        }
        if let Some(c) = &p.comment {
            pj["comment"] = json!(c);
        }
        apply_raws(&mut pj, &p.raws.iter().collect::<Vec<_>>());
        pages.push(pj);
    }
    for (name, (_, span)) in &defs {
        if !owned.contains(name) {
            diags.push(Diag::new(format!("the filter `{name}` is not shown: list it in the `show` of a `filters` component"), *span));
        }
    }
    config["pages"] = J::Array(pages);
    if !a.lets.is_empty() {
        let mut seen = HashSet::new();
        let lets: Vec<J> = a
            .lets
            .iter()
            .map(|l| {
                if !seen.insert(l.name.text.clone()) {
                    diags.push(Diag::new(format!("two `let {}`", l.name.text), l.name.span));
                }
                let mut j = json!({ "name": l.name.text, "expr": l.expr.text });
                if let Some(c) = &l.comment {
                    j["comment"] = json!(c);
                }
                j
            })
            .collect();
        config["lets"] = J::Array(lets);
    }
    apply_raws(&mut config, &a.raws.iter().collect::<Vec<_>>());
    json!({
        "kind": "app", "entity": null, "type": "app", "name": a.name,
        "description": a.description.clone().unwrap_or_default(),
        "config": config, "shares": shares_json(&a.shares),
    })
}

fn visit<'a>(blocks: &'a [Block], f: &mut impl FnMut(&'a Component)) {
    for b in blocks {
        match b {
            Block::Component(c) => f(c),
            Block::Row(r) => visit(&r.children, f),
            Block::Column(c) => visit(&c.children, f),
            Block::Space(..) => {}
        }
    }
}

/// A component as the JSON keeps it, and the filters it shows (a `filters` component).
fn lower_component(
    c: &Component,
    id: &str,
    r: [u32; 4],
    selections: &HashMap<String, String>,
    tab_ids: &mut HashSet<String>,
    diags: &mut Vec<Diag>,
) -> (J, Option<Vec<String>>) {
    let mut env = json!({ "id": id, "type": c.kind, "layout": { "x": r[0], "y": r[1], "w": r[2], "h": r[3] }, "bindings": {} });
    if let Some(t) = &c.title {
        env["title"] = json!(t);
    }
    if let Some(cm) = &c.comment {
        env["comment"] = json!(cm);
    }
    let mut cfg = json!({});
    let mut show = None;
    let mut config_raws = Vec::new();
    let mut meta_raws = Vec::new();
    let kind = c.kind.as_str();
    let mut custom = false;
    let mut mode_set = false;
    let mut sections: Vec<J> = Vec::new();
    let mut fields_line = false;
    let mut sheets: Vec<J> = Vec::new();
    for l in &c.lines {
        match l {
            Line::Opt {
                key, value, span, ..
            } => {
                let Some((opt, prefix)) = options::find(kind, &key.text) else {
                    diags.push(Diag::new(
                        format!("unknown option `{}`", key.text),
                        key.span,
                    ));
                    continue;
                };
                let path = format!("{prefix}{}", opt.path);
                match (kind, key.text.as_str(), value) {
                    ("filter", "show", Value::Names(n)) => show = Some(n.clone()),
                    ("button", "run", Value::Run(run)) => {
                        let entity = match &run.target {
                            Target::Param(n) => match selections.get(n) {
                                Some(e) => e.clone(),
                                None => {
                                    diags.push(Diag::new(format!("`@{n}` is not a record selected in the app (a view's `select`): write `<Entity>[@{n}].{}(…)`", run.action), run.span));
                                    String::new()
                                }
                            },
                            Target::Entity(e) | Target::Of(e, _) => e.clone(),
                        };
                        cfg["entity"] = json!(entity);
                        cfg["action"] = json!(run.action);
                        match &run.target {
                            Target::Param(n) => {
                                cfg["target"] = json!({ "from": "param", "name": n })
                            }
                            Target::Of(_, s) => cfg["target"] = source_json(s),
                            Target::Entity(_) => {}
                        }
                        if !run.args.is_empty() {
                            let mut args = Map::new();
                            for (n, s) in &run.args {
                                args.insert(n.clone(), source_json(s));
                            }
                            cfg["args"] = J::Object(args);
                        }
                    }
                    ("form", "fields", Value::Names(n)) => {
                        fields_line = true;
                        sections.insert(0, json!({ "fields": n }));
                    }
                    ("form", "mode", Value::Word(w)) => {
                        mode_set = true;
                        cfg["mode"] = json!(w);
                    }
                    ("form", "after", Value::After(a)) => match a {
                        After::Clear => cfg["after"] = json!("clear"),
                        After::Keep => cfg["after"] = json!("keep"),
                        After::Open => cfg["after"] = json!("open"),
                        After::Select { entity, name } => {
                            cfg["after"] = json!("select");
                            cfg["select"] = json!({ "name": name, "entity": entity });
                        }
                    },
                    (_, _, v) => match value_json(v) {
                        Some(j) => set_dotted(&mut cfg, &path, j),
                        None => diags.push(Diag::new(
                            format!("`{}`: this value does not fit here", key.text),
                            *span,
                        )),
                    },
                }
            }
            Line::Apply { entity, binds, .. } => {
                for (n, p) in binds {
                    let v = p.as_ref().map_or(J::Null, |p| json!(p.text));
                    set(&mut env, &["bindings", "filters", &entity.text, &n.text], v);
                }
            }
            Line::ApplyAuto(b, _) => set(&mut env, &["bindings", "filters", "auto"], json!(b)),
            Line::Compute { name, expr, span } => {
                if kind != "view" {
                    diags.push(Diag::new("`compute` is a view's", *span));
                }
                set(&mut cfg, &["view", "columns", &name.text], json!(expr.text));
            }
            Line::Color { color, expr, span } => {
                if kind != "view" {
                    diags.push(Diag::new("`color` is a view's", *span));
                }
                push(
                    &mut cfg,
                    &["view", "colors"],
                    json!({ "expr": expr.text, "color": color }),
                );
            }
            Line::Default { field, source, .. } => {
                set(&mut cfg, &["defaults", &field.text], source_json(source))
            }
            Line::Section { title, fields, .. } => {
                let mut s = json!({ "fields": fields });
                if let Some(t) = title {
                    s["title"] = json!(t);
                }
                sections.push(s);
            }
            Line::Field(f) => {
                custom = true;
                let mut j = json!({ "name": f.name.text, "type": f.ty.text });
                if let Some(l) = &f.label {
                    j["label"] = json!(l);
                }
                if let Some(w) = &f.where_ {
                    j["where"] = json!(w.text);
                }
                if let Some(d) = &f.default {
                    j["default"] = json!(d.text);
                }
                if let Some(cm) = &f.comment {
                    j["comment"] = json!(cm);
                }
                push(&mut cfg, &["fields"], j);
            }
            Line::Submit { label, code, .. } => {
                custom = true;
                cfg["submit"] = json!(code);
                if let Some(l) = label {
                    cfg["submit_label"] = json!(l);
                }
            }
            Line::Tab(t) => sheets.push(lower_tab(t, tab_ids)),
            Line::Text(t, _) => cfg["text"] = json!(t),
            Line::Raw(r) => {
                if r.meta {
                    meta_raws.push(r);
                } else {
                    config_raws.push(r);
                }
            }
        }
    }
    if !sections.is_empty() {
        if fields_line && sections.len() > 1 {
            diags.push(Diag::new("`fields` is a form's one section: with `section` lines, every field is in a section", c.span));
        }
        cfg["sections"] = J::Array(sections);
    }
    if custom && !mode_set {
        cfg["mode"] = json!("custom");
    }
    if !sheets.is_empty() {
        cfg["sheets"] = J::Array(sheets);
    }
    if let Some(view) = cfg.get_mut("view") {
        complete_view_parts(view, c.span, diags);
    }
    apply_raws(&mut cfg, &config_raws);
    env["config"] = cfg;
    apply_raws(&mut env, &meta_raws);
    (env, show)
}

fn push(obj: &mut J, path: &[&str], v: J) {
    let mut cur = obj;
    for p in path {
        if !cur.is_object() {
            *cur = json!({});
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(p.to_string())
            .or_insert(J::Null);
    }
    if !cur.is_array() {
        *cur = json!([]);
    }
    cur.as_array_mut().unwrap().push(v);
}

/// A board's `kanban` and a view's `group` are kept whole by the client: the keys a file does
/// not write get their defaults.
fn complete_view_parts(view: &mut J, span: Span, diags: &mut Vec<Diag>) {
    if let Some(k) = view.get_mut("kanban").and_then(J::as_object_mut) {
        if !k.contains_key("field") {
            diags.push(Diag::new(
                "a board's options need `by` (the field of its columns)",
                span,
            ));
        }
        k.entry("cards").or_insert(json!([]));
        k.entry("hidden").or_insert(json!([]));
        k.entry("collapsed").or_insert(json!([]));
        k.entry("wip").or_insert(json!({}));
    }
    if let Some(g) = view.get_mut("group").and_then(J::as_object_mut) {
        if !g.contains_key("keys") {
            diags.push(Diag::new(
                "a view's grouping needs `group` (its keys)",
                span,
            ));
        }
        g.entry("aggregates").or_insert(json!({}));
        g.entry("pivot").or_insert(json!(false));
        g.entry("order")
            .or_insert(json!({ "by": "key", "desc": false }));
        g.entry("measure").or_insert(json!("count"));
    }
}

/// The JSON of an option's value (the generic ones).
fn value_json(v: &Value) -> Option<J> {
    Some(match v {
        Value::Expr(e) => json!(e.text),
        Value::Text(t) => json!(t),
        Value::Int(n) => json!(n),
        Value::Bool(b) => json!(b),
        Value::Word(w) => json!(w),
        Value::Names(n) => json!(n),
        Value::Pairs(p) => J::Object(p.iter().cloned().collect()),
        Value::Data(d) => d.clone(),
        Value::Source(DataSource::Expr(e)) => json!({ "type": "expr", "expr": e.text }),
        Value::Source(DataSource::Cell { component, cell }) => {
            json!({ "type": "cell", "component": component, "cell": cell })
        }
        Value::Source(DataSource::Range { component, range }) => {
            json!({ "type": "range", "component": component, "range": range })
        }
        Value::Compare(Compare::Previous(f)) => json!({ "kind": "previous_period", "filter": f }),
        Value::Compare(Compare::Target(e)) => json!({ "kind": "target", "expr": e.text }),
        Value::From(s) => source_json(s),
        Value::Sort(s) => J::Array(
            s.iter()
                .map(|(f, d)| json!({ "field": f, "desc": d }))
                .collect(),
        ),
        Value::Group(g) => J::Array(
            g.iter()
                .map(|(f, b)| match b {
                    Some(b) => json!({ "field": f, "bucket": b }),
                    None => json!({ "field": f }),
                })
                .collect(),
        ),
        Value::Order(by, desc) => json!({ "by": by, "desc": desc }),
        Value::Shares(s) => shares_json(s),
        Value::Flags(f) => J::Object(f.iter().map(|k| (k.clone(), J::Bool(true))).collect()),
        Value::Run(_) | Value::After(_) => return None,
    })
}

// ---- spreadsheets ----------------------------------------------------------------------------------

fn lower_tab(t: &Tab, ids: &mut HashSet<String>) -> J {
    let raws: Vec<Raw> = t
        .lines
        .iter()
        .filter_map(|l| {
            if let TabLine::Raw(r) = l {
                Some(r.clone())
            } else {
                None
            }
        })
        .collect();
    let id = match (&t.id, meta_id(&raws)) {
        (Some(n), _) => {
            ids.insert(n.text.clone());
            n.text.clone()
        }
        (None, Some(id)) => id,
        (None, None) => fresh("sheet", ids),
    };
    let mut s = json!({ "id": id, "cells": {} });
    if let Some(n) = &t.name {
        s["name"] = json!(n);
    }
    if let Some(m) = &t.mode {
        s["mode"] = json!(m);
    }
    if let Some(r) = t.rows {
        s["rows"] = json!(r);
    }
    if let Some(c) = t.cols {
        s["cols"] = json!(c);
    }
    if let Some(c) = &t.comment {
        s["comment"] = json!(c);
    }
    for l in &t.lines {
        match l {
            TabLine::Cell {
                addr, style, value, ..
            } => {
                let mut cell = json!({});
                match value {
                    Some(CellValue::Text(v)) => cell["v"] = json!(v),
                    Some(CellValue::Formula(f, _)) => cell["v"] = json!(format!("={f}")),
                    None => {}
                }
                if !style.is_empty() {
                    let mut st = json!({});
                    for it in style {
                        match it {
                            StyleItem::Flag(f) => {
                                let k = match f.as_str() {
                                    "bold" => "b",
                                    "italic" => "i",
                                    "underline" => "u",
                                    "strike" => "st",
                                    _ => "wrap",
                                };
                                st[k] = json!(true);
                            }
                            StyleItem::Color(k, c) => {
                                st[if k == "color" { "fg" } else { "bg" }] = json!(c)
                            }
                            StyleItem::Align(k, a) => {
                                st[if k == "align" { "ha" } else { "va" }] = json!(a)
                            }
                            StyleItem::Format {
                                kind,
                                dec,
                                thousands,
                                date,
                            } => {
                                let mut f = json!({ "kind": kind });
                                if let Some(d) = dec {
                                    f["dec"] = json!(d);
                                }
                                if *thousands {
                                    f["thou"] = json!(true);
                                }
                                if let Some(d) = date {
                                    f["date"] = json!(d);
                                }
                                st["fmt"] = f;
                            }
                            StyleItem::Border(b) => st["bd"] = b.clone(),
                            StyleItem::Style(J::Object(o)) => {
                                for (k, v) in o {
                                    st[k.as_str()] = v.clone();
                                }
                            }
                            StyleItem::Style(other) => st = other.clone(),
                        }
                    }
                    cell["s"] = st;
                }
                s["cells"][addr.as_str()] = cell;
            }
            TabLine::Widths(p, _) => s["cols_w"] = J::Object(p.iter().cloned().collect()),
            TabLine::Heights(p, _) => s["rows_h"] = J::Object(p.iter().cloned().collect()),
            TabLine::Merge(m, _) => s["merges"] = json!(m),
            TabLine::Freeze { rows, cols, .. } => {
                let mut f = json!({});
                if let Some(r) = rows {
                    f["rows"] = json!(r);
                }
                if let Some(c) = cols {
                    f["cols"] = json!(c);
                }
                s["freeze"] = f;
            }
            TabLine::Raw(_) => {}
        }
    }
    apply_raws(&mut s, &raws.iter().collect::<Vec<_>>());
    s
}

// ---- saved views -----------------------------------------------------------------------------------

fn lower_saved_view(v: &SavedView, diags: &mut Vec<Diag>) -> J {
    let mut def = json!({ "kind": "view", "entity": v.entity.text, "type": "grid", "name": v.name, "description": "", "shares": [] });
    let mut cfg = json!({});
    if let Some(c) = &v.comment {
        cfg["comment"] = json!(c);
    }
    let mut config_raws = Vec::new();
    let mut meta_raws = Vec::new();
    for l in &v.lines {
        match l {
            Line::Opt {
                key, value, span, ..
            } => match (key.text.as_str(), value) {
                ("description", Value::Text(t)) => def["description"] = json!(t),
                ("share", Value::Shares(s)) => def["shares"] = shares_json(s),
                ("type", Value::Word(w)) => def["type"] = json!(w),
                (k, v) => match (options::find_saved(k), value_json(v)) {
                    (Some(o), Some(j)) => set_dotted(&mut cfg, o.path, j),
                    _ => diags.push(Diag::new(format!("`{k}`: not an option of a view"), *span)),
                },
            },
            Line::Compute { name, expr, .. } => {
                set(&mut cfg, &["columns", &name.text], json!(expr.text))
            }
            Line::Color { color, expr, .. } => push(
                &mut cfg,
                &["colors"],
                json!({ "expr": expr.text, "color": color }),
            ),
            Line::Raw(r) => {
                if r.meta {
                    meta_raws.push(r);
                } else {
                    config_raws.push(r);
                }
            }
            other => diags.push(Diag::new("not a line of a view", line_span(other))),
        }
    }
    complete_view_parts(&mut cfg, v.span, diags);
    apply_raws(&mut cfg, &config_raws);
    def["config"] = cfg;
    apply_raws(&mut def, &meta_raws);
    def
}

fn line_span(l: &Line) -> Span {
    match l {
        Line::Opt { span, .. }
        | Line::Apply { span, .. }
        | Line::Compute { span, .. }
        | Line::Color { span, .. }
        | Line::Default { span, .. }
        | Line::Section { span, .. }
        | Line::Submit { span, .. } => *span,
        Line::ApplyAuto(_, s) | Line::Text(_, s) => *s,
        Line::Field(f) => f.span,
        Line::Tab(t) => t.span,
        Line::Raw(r) => r.span,
    }
}
