//! Completion in interface files (`.qdi`): the options of the block the cursor is in, the
//! entities, the fields of the entity a block is about, the app's `@parameters`, the fields of
//! a lambda's record (`Order.where(o => o.|`), and the methods of the language after a `.`.

use quadra_qdi::options;
use std::collections::HashMap;

/// A model as completion needs it: entity → (fields, actions).
pub type Entities = HashMap<String, (Vec<String>, Vec<String>)>;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    pub kind: &'static str,
    pub detail: String,
}

fn item(label: impl Into<String>, kind: &'static str, detail: impl Into<String>) -> Item {
    Item { label: label.into(), kind, detail: detail.into() }
}

fn indent(l: &str) -> usize {
    l.chars().take_while(|c| *c == ' ').count()
}

/// The lines above the cursor's that hold it: (indentation, first word, the line), outermost
/// last.
fn enclosing<'a>(lines: &[&'a str], at: usize) -> Vec<(usize, &'a str, &'a str)> {
    let mut out = Vec::new();
    let mut limit = indent(lines[at]);
    // the cursor's own line counts when it is not blank
    for l in lines[..at].iter().rev() {
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let i = indent(l);
        if i < limit {
            let word = l.trim_start().split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("");
            out.push((i, word, *l));
            limit = i;
            if i == 0 {
                break;
            }
        }
    }
    out
}

/// The JSON type of a component header (`kpi a "A"` → `kpi`, `filters` → `filter`).
fn header_kind(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let w = words.next()?;
    if w == "component" {
        return words.next().map(str::to_string);
    }
    options::KINDS.contains(&w).then(|| options::json_type(w).to_string())
}

/// The entity a block is about: its `of:` / `entity:` line, or a saved view's header.
fn block_entity(lines: &[&str], header: usize) -> Option<String> {
    let h = lines[header];
    if let Some(rest) = h.trim_start().strip_prefix("view \"") {
        if let Some((_, after)) = rest.split_once("\" of ") {
            return Some(after.trim_end_matches(':').trim().to_string());
        }
    }
    let base = indent(h);
    for l in &lines[header + 1..] {
        if !l.trim().is_empty() && indent(l) <= base {
            break;
        }
        let t = l.trim_start();
        for k in ["of:", "entity:"] {
            if let Some(e) = t.strip_prefix(k) {
                return Some(e.trim().to_string());
            }
        }
    }
    None
}

/// The record a lambda's parameter is of: `Order.where(o => o.` → `Order` for `o`.
fn lambda_entities(text: &str, entities: &Entities) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut k = 0;
    while let Some(p) = text[k..].find("=>") {
        let at = k + p;
        // the parameter before `=>`
        let mut j = at;
        while j > 0 && b[j - 1] == b' ' {
            j -= 1;
        }
        let end = j;
        while j > 0 && ident(b[j - 1]) {
            j -= 1;
        }
        let param = &text[j..end];
        // `Entity.method(` before it
        let before = text[..j].trim_end();
        if let Some(before) = before.strip_suffix('(') {
            let before = before.trim_end();
            let m_end = before.len();
            let m_start = before.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(0, |x| x + 1);
            let recv = before[..m_start].strip_suffix('.').map(|r| {
                let s = r.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(0, |x| x + 1);
                &r[s..]
            });
            let _ = m_end;
            if let Some(e) = recv.filter(|e| entities.contains_key(*e)) {
                if !param.is_empty() {
                    out.insert(param.to_string(), e.to_string());
                }
            }
        }
        k = at + 2;
    }
    out
}

/// What may be written at `offset` of an interface file.
pub fn complete(text: &str, offset: usize, entities: &Entities, params: &[(String, Option<String>)]) -> Vec<Item> {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |k| k + 1);
    let line = &before[line_start..];
    let typed_start = line.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(0, |k| k + 1);
    let typed = &line[typed_start..];
    let head = &line[..typed_start];
    let matches = |s: &str| s.starts_with(typed);
    let mut out = Vec::new();
    // `@name`
    if head.ends_with('@') {
        for (n, t) in params {
            if matches(n) && !n.ends_with("__on") {
                out.push(item(n.clone(), "variable", t.clone().unwrap_or_default()));
            }
        }
        return out;
    }
    let lines: Vec<&str> = text[..].split('\n').collect();
    let at = before.matches('\n').count();
    // after a `.`: the fields of a lambda's record or of a selection, the methods
    if head.ends_with('.') {
        let recv_end = head.len() - 1;
        let recv_start = head[..recv_end].rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(0, |k| k + 1);
        let recv = &head[recv_start..recv_end];
        let entity = if head[..recv_start].ends_with('@') {
            params.iter().find(|p| p.0 == recv).and_then(|p| p.1.as_deref()).map(|t| t.trim_end_matches('?').to_string())
        } else {
            // the expression may start on a line above (a chain): its whole text
            lambda_entities(before, entities).get(recv).cloned()
        };
        if let Some((fields, actions)) = entity.as_deref().and_then(|e| entities.get(e)) {
            out.extend(fields.iter().filter(|f| matches(f)).map(|f| item(f.clone(), "field", entity.clone().unwrap_or_default())));
            out.extend(actions.iter().filter(|f| matches(f)).map(|f| item(f.clone(), "method", "action")));
        }
        out.extend(quadra_sema::ide::method_completions(before, (offset as u32).into()).into_iter().map(|(n, s, _)| item(n, "method", format!(".{s}"))));
        return out;
    }
    let ctx = enclosing(&lines, at);
    let header_line = |k: usize| lines.iter().position(|l| std::ptr::eq(*l, ctx[k].2));
    // the start of a line: the options of the block
    if head.trim().is_empty() {
        match ctx.first() {
            None => out.extend(["app", "view"].iter().map(|k| item(*k, "keyword", "an app, a saved view"))),
            Some((_, "app", _)) => out.extend(["description", "share", "filter", "let", "page", "meta"].iter().map(|k| item(*k, "keyword", "of an app"))),
            Some((_, "page", _)) => out.extend(["row", "space", "meta"].iter().map(|k| item(*k, "keyword", "of a page"))),
            Some((_, "row" | "column", _)) => {
                out.extend(options::KINDS.iter().map(|k| item(*k, "class", "a component")));
                out.push(item(if ctx[0].1 == "row" { "column" } else { "row" }, "keyword", ""));
                out.push(item("space", "keyword", ""));
            }
            Some((_, "view", l)) if l.trim_start().starts_with("view \"") => {
                out.extend(options::SAVED_VIEW.iter().chain(options::VIEW_BODY).map(|o| item(o.key, "property", o.doc)));
                out.extend(["compute", "color", "config", "meta"].iter().map(|k| item(*k, "keyword", "")));
            }
            Some((_, _, l)) => {
                if let Some(kind) = header_kind(l) {
                    let mut keys: Vec<(&str, &str)> = options::table(&kind).iter().map(|o| (o.key, o.doc)).collect();
                    if kind == "view" {
                        keys.extend(options::VIEW_BODY.iter().map(|o| (o.key, o.doc)));
                        keys.extend([("compute", ""), ("color", "")]);
                    }
                    if kind == "form" {
                        keys.extend([("field", "a field of a custom form"), ("submit", "what a custom form runs"), ("section", ""), ("default", "")]);
                    }
                    if kind == "sheet" {
                        keys.push(("tab", "a sheet"));
                    }
                    keys.extend([("apply", "filters on another entity"), ("config", ""), ("meta", "")]);
                    out.extend(keys.into_iter().map(|(k, d)| item(k, "property", d)));
                }
            }
        }
        out.retain(|i| matches(&i.label));
        return out;
    }
    // after `key:`: entities, fields of the block's entity, words of the option
    let key = head.trim_start().split(':').next().unwrap_or("").trim();
    let header = ctx.first().and_then(|_| header_line(0));
    let entity = header.and_then(|h| block_entity(&lines, h));
    let words: &[&str] = match key {
        "format" => options::KPI_STYLES,
        "kind" => options::CHART_KINDS,
        "style" => options::BUTTON_STYLES,
        "mode" => options::FORM_MODES,
        "type" => options::VIEW_TYPES,
        "layout" => options::FILTER_LAYOUTS,
        "edit" => options::VIEW_EDITS,
        _ => &[],
    };
    out.extend(words.iter().filter(|w| matches(w)).map(|w| item(*w, "enum", key)));
    match key {
        "of" | "entity" => {
            let mut names: Vec<&String> = entities.keys().collect();
            names.sort();
            out.extend(names.into_iter().filter(|e| matches(e)).map(|e| item(e.clone(), "class", "entity")));
        }
        "fields" | "cards" | "by" | "sum" | "card_title" | "sort" | "group" | "widths" | "measure" | "aggregates" => {
            if let Some((fields, _)) = entity.as_deref().and_then(|e| entities.get(e)) {
                out.extend(fields.iter().filter(|f| matches(f)).map(|f| item(f.clone(), "field", entity.clone().unwrap_or_default())));
            }
        }
        "show" => {
            // the app's filters
            for l in &lines {
                if let Some(rest) = l.trim_start().strip_prefix("filter ") {
                    let n = rest.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
                    if matches(n) && !n.is_empty() {
                        out.push(item(n, "variable", "filter"));
                    }
                }
            }
        }
        _ => {
            // an expression: the entities, the lets
            let mut names: Vec<&String> = entities.keys().collect();
            names.sort();
            if !typed.is_empty() {
                out.extend(names.into_iter().filter(|e| matches(e)).map(|e| item(e.clone(), "class", "entity")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Entities {
        let mut e = Entities::new();
        e.insert("Order".into(), (vec!["number".into(), "total".into(), "customer".into()], vec!["confirm".into()]));
        e.insert("Customer".into(), (vec!["name".into()], vec![]));
        e
    }

    fn labels(text: &str) -> Vec<String> {
        let at = text.find('|').unwrap();
        let t = text.replace('|', "");
        complete(&t, at, &model(), &[("order".into(), Some("Order?".into())), ("region".into(), Some("[Region]".into()))]).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn completion() {
        let app = "app \"A\":\n    page p:\n        row height 2:\n            kpi k width 4:\n                so|\n";
        assert_eq!(labels(app), vec!["source"]);
        let view = "app \"A\":\n    page p:\n        row height 2:\n            view v width 4:\n                of: Order\n                fields: num|\n";
        assert_eq!(labels(view), vec!["number"]);
        let ent = "app \"A\":\n    page p:\n        row height 2:\n            view v width 4:\n                of: Or|\n";
        assert_eq!(labels(ent), vec!["Order"]);
        let lambda = "app \"A\":\n    let x = Order.where(o => o.to|\n";
        assert!(labels(lambda).contains(&"total".to_string()));
        let param = "app \"A\":\n    let x = @or|\n";
        assert_eq!(labels(param), vec!["order"]);
        let sel = "app \"A\":\n    let x = @order.nu|\n";
        assert!(labels(sel).contains(&"number".to_string()));
        let saved = "view \"V\" of Customer:\n    fields: na|\n";
        assert_eq!(labels(saved), vec!["name"]);
    }
}
