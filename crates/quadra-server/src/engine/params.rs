//! The parameters of an app's formulas: its global filters and the records selected in its
//! embedded views, as `@name` in formulas (`POST /v1/eval`, `params` and `bind`).
//!
//! A parameter is declared by what it filters, never by its value: `{"name": "region",
//! "entity": "Order", "field": "customer.region", "widget": "multi", "value": [3, 5]}`. Its
//! type comes from the field (`[Region]`), so it is a typed parameter of the workbook's
//! function (`fn __sheet(__p_region: [Region], …)`): `@region` is checked like any name, and
//! changing a filter's value changes nothing the checker sees (the checked workbook is reused,
//! only the formulas are evaluated again).
//!
//! What each widget gives formulas:
//!
//! | widget         | names                         | type (T: the field's)    | nothing chosen     |
//! |----------------|-------------------------------|--------------------------|--------------------|
//! | `select`       | `@name`                       | `T?`                     | `none`             |
//! | `multi`        | `@name`                       | `[T]`                    | `[]`               |
//! | `record`       | `@name` (a selected record)   | `Entity?`                | `none`             |
//! | `date_range`   | `@name`                       | `DateRange`              | the whole calendar |
//! | `number_range` | `@name_min`, `@name_max`      | `T?`                     | `none`             |
//! | `toggle`       | `@name`                       | `Bool`                   | `false`            |
//! | `text`         | `@name`                       | `Text`                   | `""`               |
//!
//! `bind` applies filters to whole entities: `{"Order": {"region": "customer.region"}}` makes
//! every `Order.…` of the formulas `Order.where(__q => (__p_region.count() == 0 or
//! __q.customer.region in __p_region)).…` — the condition is the same text whatever the
//! values (a filter not set holds for every row), so it is checked once; its parts that do not
//! depend on the row are computed before the query, which stays one SQL query.

use super::*;
use quadra_ir::Ty;
use std::collections::HashSet;

/// A parameter as the workbook's function declares it.
#[derive(Clone, Debug)]
pub struct Param {
    /// the name formulas use after `@` (`region`, `amount_min`)
    pub name: String,
    /// the parameter of `fn __sheet` (`__p_region`)
    pub var: String,
    /// its type as the language writes it (None: not written, e.g. a choice declared inline)
    pub ty: Option<String>,
    pub value: V,
}

/// A filter of the request, with what `bind` needs of it.
struct Filter {
    widget: String,
    var: String,
}

/// The parameters of a request and the condition each bound entity gets.
#[derive(Default)]
pub struct Params {
    pub list: Vec<Param>,
    /// entity → the text that replaces `Entity.` in formulas (`Order.where(__q => …).`)
    pub scopes: Vec<(String, String)>,
}

pub const WIDGETS: &[&str] = &["select", "multi", "record", "date_range", "number_range", "toggle", "text"];

pub fn name_ok(n: &str) -> bool {
    let mut cs = n.chars();
    matches!(cs.next(), Some('a'..='z')) && n.len() <= 30 && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The fields of a path from `entity` (`customer.region`): each with whether it may be empty;
/// the type at its end. Every field but the last is a reference to an entity.
pub fn walk_path(model: &Model, entity: &str, path: &str) -> Result<(Vec<(String, bool)>, Ty), String> {
    let segs: Vec<&str> = path.split('.').collect();
    if path.is_empty() || segs.len() > 5 || segs.iter().any(|s| s.is_empty()) {
        return Err(format!("`{path}`: a field path of at most 5 fields"));
    }
    let mut ent = entity.to_string();
    let mut out = Vec::new();
    let mut last: Option<Ty> = None;
    for (k, seg) in segs.iter().enumerate() {
        let e = model.entity(&ent).ok_or_else(|| format!("`{path}`: `{ent}` is not an entity"))?;
        let f = model.find_field(e, seg).ok_or_else(|| format!("`{path}`: `{ent}` has no field `{seg}`"))?;
        let ty = f.ty.clone().ok_or_else(|| format!("`{path}`: `{ent}.{seg}` has no declared type"))?;
        let optional = matches!(ty, Ty::Optional(_));
        if k + 1 < segs.len() {
            let target = match &ty {
                Ty::Optional(t) => named(t),
                t => named(t),
            }
            .filter(|n| model.entity(n).is_some_and(|x| x.kind == EntityKind::Entity))
            .ok_or_else(|| format!("`{path}`: `{ent}.{seg}` is not a reference"))?;
            ent = target;
        }
        out.push((seg.to_string(), optional));
        last = Some(ty);
    }
    Ok((out, last.unwrap()))
}

fn named(t: &Ty) -> Option<String> {
    match t {
        Ty::Path { segments, .. } => segments.last().cloned(),
        _ => None,
    }
}

fn unopt(t: &Ty) -> &Ty {
    match t {
        Ty::Optional(i) => i,
        t => t,
    }
}

/// How the language writes a type, when a parameter can be declared with it.
fn type_text(t: &Ty) -> Option<String> {
    let t = unopt(t);
    if quadra_ir::inline_enum(t).is_some() || matches!(t, Ty::List(_) | Ty::Set(_)) {
        return None;
    }
    Some(quadra_ir::print::ty(t))
}

/// A value of the request for a field of type `t`: as `/v1/tx` reads it, and a plain number
/// for an amount (`1000` for a `Money<EUR>` is `1000 EUR`).
pub fn decode_value(i: &mut Interp, model: &Model, t: &Ty, j: &J) -> Result<V, String> {
    let t = unopt(t);
    let fixed = match t {
        Ty::Path { segments, args, .. } if matches!(segments.last().map(String::as_str), Some("Money" | "Qty")) && !args.is_empty() => match j {
            J::Number(n) => Some(json!(format!("{n} {}", quadra_ir::print::ty(&args[0])))),
            J::String(s) if s.trim().parse::<f64>().is_ok() => Some(json!(format!("{} {}", s.trim(), quadra_ir::print::ty(&args[0])))),
            _ => None,
        },
        Ty::Path { segments, .. } if segments.last().map(String::as_str) == Some("Date") => match j {
            J::String(s) => return chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(V::Date).map_err(|_| format!("`{s}` is not a date (YYYY-MM-DD)")),
            _ => None,
        },
        _ => None,
    };
    quadra_api::json::decode(i, model, Some(t), fixed.as_ref().unwrap_or(j), &|_| None)
}

fn date_of(j: &J, end: bool) -> Result<chrono::NaiveDate, String> {
    match j {
        J::Null => Ok(if end { chrono::NaiveDate::from_ymd_opt(9999, 12, 31).unwrap() } else { chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap() }),
        J::String(s) => chrono::NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").map_err(|_| format!("`{s}` is not a date (YYYY-MM-DD)")),
        _ => Err("a date is text: YYYY-MM-DD".into()),
    }
}

/// The parameters and the bound entities of a request (`params`, `bind`), against the model,
/// their values read as the user (`i`).
pub fn params_of(i: &mut Interp, model: &Model, body: &J) -> Result<Params, Fail> {
    let bad = |m: String| (400, m);
    let mut out = Params::default();
    let mut filters: HashMap<String, (Filter, Ty)> = HashMap::new();
    let list = match &body["params"] {
        J::Null => Vec::new(),
        J::Array(a) => a.clone(),
        _ => return Err(bad("`params`: a list of {name, entity, field, widget, value}".into())),
    };
    if list.len() > 50 {
        return Err(bad("at most 50 parameters".into()));
    }
    let mut taken: HashSet<String> = HashSet::new();
    for p in &list {
        let name = p["name"].as_str().unwrap_or("");
        if !name_ok(name) {
            return Err(bad(format!("parameter `{name}`: a name of lower-case letters, digits and _ (at most 30), starting with a letter")));
        }
        let at = |m: String| bad(format!("parameter `{name}`: {m}"));
        let widget = p["widget"].as_str().unwrap_or("select");
        if !WIDGETS.contains(&widget) {
            return Err(at(format!("`widget`: one of {}", WIDGETS.join(", "))));
        }
        let entity = p["entity"].as_str().unwrap_or("");
        if model.entity(entity).map_or(true, |e| e.kind != EntityKind::Entity) {
            return Err(at(format!("`{entity}` is not an entity")));
        }
        let ty: Ty = if widget == "record" {
            Ty::Path { segments: vec![entity.to_string()], args: Vec::new(), span: model.entity(entity).unwrap().span }
        } else {
            let field = p["field"].as_str().unwrap_or("");
            walk_path(model, entity, field).map_err(at)?.1
        };
        let base = unopt(&ty).clone();
        let tt = type_text(&base);
        let v = &p["value"];
        let var = format!("__p_{name}");
        let mut add = |n: String, var: String, ty: Option<String>, value: V| -> Result<(), Fail> {
            if !taken.insert(n.clone()) {
                return Err(bad(format!("two parameters named `{n}`")));
            }
            out.list.push(Param { name: n, var, ty, value });
            Ok(())
        };
        match widget {
            "select" | "record" => {
                let value = if v.is_null() { V::None } else { decode_value(i, model, &base, v).map_err(at)? };
                add(name.into(), var.clone(), tt.map(|t| format!("{t}?")), value)?;
            }
            "multi" => {
                let items = match v {
                    J::Null => Vec::new(),
                    J::Array(a) if a.len() <= 500 => a.clone(),
                    J::Array(_) => return Err(at("at most 500 values".into())),
                    other => vec![other.clone()],
                };
                let mut vals = Vec::new();
                for it in &items {
                    vals.push(decode_value(i, model, &base, it).map_err(at)?);
                }
                add(name.into(), var.clone(), tt.map(|t| format!("[{t}]")), V::List(Rc::new(vals)))?;
            }
            "date_range" => {
                let is_date = matches!(named(&base).as_deref(), Some("Date" | "DateTime"));
                if !is_date {
                    return Err(at("a date range filters a date".into()));
                }
                let (a, b, on) = match v {
                    J::Null => (date_of(&J::Null, false).map_err(at)?, date_of(&J::Null, true).map_err(at)?, false),
                    J::Array(x) if x.len() == 2 => {
                        let on = !(x[0].is_null() && x[1].is_null());
                        (date_of(&x[0], false).map_err(at)?, date_of(&x[1], true).map_err(at)?, on)
                    }
                    _ => return Err(at("a date range is [from, to] (either may be null)".into())),
                };
                if a > b {
                    return Err(at("the range ends before it starts".into()));
                }
                add(name.into(), var.clone(), Some("DateRange".into()), V::DateRange(a, b))?;
                add(format!("{name}__on"), format!("{var}__on"), Some("Bool".into()), V::Bool(on))?;
            }
            "number_range" => {
                let numeric = matches!(named(&base).as_deref(), Some("Int" | "Decimal" | "Money" | "Qty" | "Percent" | "Price"));
                if !numeric {
                    return Err(at("a number range filters a number or an amount".into()));
                }
                let (lo, hi) = match v {
                    J::Null => (J::Null, J::Null),
                    J::Array(x) if x.len() == 2 => (x[0].clone(), x[1].clone()),
                    _ => return Err(at("a number range is [min, max] (either may be null)".into())),
                };
                let lo = if lo.is_null() { V::None } else { decode_value(i, model, &base, &lo).map_err(at)? };
                let hi = if hi.is_null() { V::None } else { decode_value(i, model, &base, &hi).map_err(at)? };
                add(format!("{name}_min"), format!("{var}_min"), tt.clone().map(|t| format!("{t}?")), lo)?;
                add(format!("{name}_max"), format!("{var}_max"), tt.map(|t| format!("{t}?")), hi)?;
            }
            "toggle" => {
                if named(&base).as_deref() != Some("Bool") {
                    return Err(at("a toggle filters a yes/no field".into()));
                }
                add(name.into(), var.clone(), Some("Bool".into()), V::Bool(v.as_bool().unwrap_or(false)))?;
            }
            _ => {
                let text = v.as_str().unwrap_or("");
                if text.chars().count() > 200 {
                    return Err(at("at most 200 characters".into()));
                }
                if !quadra_runtime::query::text_type(Some(&base)) {
                    return Err(at("a text filter searches a text field".into()));
                }
                add(name.into(), var.clone(), Some("Text".into()), V::Text(Rc::from(text.trim())))?;
            }
        }
        filters.insert(name.to_string(), (Filter { widget: widget.to_string(), var }, base));
    }
    // the entities filters apply to
    if let Some(b) = body["bind"].as_object() {
        for (entity, map) in b.iter().take(50) {
            if model.entity(entity).map_or(true, |e| e.kind != EntityKind::Entity) {
                return Err(bad(format!("`bind`: `{entity}` is not an entity")));
            }
            let Some(map) = map.as_object() else {
                return Err(bad(format!("`bind.{entity}`: an object filter → field path")));
            };
            let mut conds = Vec::new();
            for (fname, path) in map {
                let Some(path) = path.as_str() else { continue };
                let (f, home) = filters.get(fname).ok_or_else(|| bad(format!("`bind.{entity}`: no parameter `{fname}`")))?;
                let (segs, end) = walk_path(model, entity, path).map_err(|m| bad(format!("`bind.{entity}.{fname}`: {m}")))?;
                // the path from the row: `__q.customer?.region`
                let mut p = String::from("__q");
                for (k, (seg, _)) in segs.iter().enumerate() {
                    let opt_before = k > 0 && segs[k - 1].1;
                    p.push_str(if opt_before { "?." } else { "." });
                    p.push_str(seg);
                }
                let end_opt = segs.last().is_some_and(|s| s.1) || segs.iter().any(|s| s.1);
                let end_t = unopt(&end).clone();
                // the value at the end must be of the filter's kind
                let same = f.widget == "text" || f.widget == "toggle" || named(&end_t) == named(home) || f.widget == "date_range" || f.widget == "number_range";
                if !same {
                    return Err(bad(format!("`bind.{entity}.{fname}`: `{path}` is a {}, the filter chooses a {}", quadra_ir::print::ty(&end_t), quadra_ir::print::ty(home))));
                }
                let v = &f.var;
                conds.push(match f.widget.as_str() {
                    "select" | "record" => format!("({v} == none or {p} == {v})"),
                    "multi" => format!("({v}.count() == 0 or {p} in {v})"),
                    "date_range" => {
                        if end_opt {
                            format!("(not {v}__on or ({p} != none and {p} in {v}))")
                        } else {
                            format!("(not {v}__on or {p} in {v})")
                        }
                    }
                    "number_range" => format!("({v}_min == none or {p} >= {v}_min) and ({v}_max == none or {p} <= {v}_max)"),
                    "toggle" => format!("(not {v} or {p} == true)"),
                    _ => {
                        if end_opt {
                            format!("({v} == \"\" or ({p} ?? \"\").contains({v}))")
                        } else {
                            format!("({v} == \"\" or {p}.contains({v}))")
                        }
                    }
                });
            }
            if !conds.is_empty() {
                out.scopes.push((entity.clone(), format!("{entity}.where(__q => {}).", conds.join(" and "))));
            }
        }
    }
    Ok(out)
}

/// `@name` → the workbook's parameter (outside texts and comments); an unknown name is an
/// error of the formula.
pub fn rewrite_params(src: &str, params: &[Param]) -> Result<String, String> {
    let ch: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len() + 16);
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < ch.len() && ch[i] != '"' {
                if ch[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(ch.len());
            out.extend(&ch[start..i]);
            continue;
        }
        if c == '#' {
            out.extend(&ch[i..]);
            break;
        }
        if c == '@' {
            let start = i + 1;
            let mut j = start;
            while j < ch.len() && (ch[j].is_ascii_alphanumeric() || ch[j] == '_') {
                j += 1;
            }
            let name: String = ch[start..j].iter().collect();
            if name.is_empty() {
                return Err("`@` is followed by the name of a filter or of a selected record (`@region`)".into());
            }
            match params.iter().find(|p| p.name == name && !p.name.ends_with("__on")) {
                Some(p) => out.push_str(&p.var),
                None => {
                    let known: Vec<String> = params.iter().filter(|p| !p.name.ends_with("__on")).map(|p| format!("@{}", p.name)).collect();
                    return Err(if known.is_empty() {
                        format!("`@{name}`: this app has no filter or selected record of that name")
                    } else {
                        format!("`@{name}`: no filter or selected record of that name (there are {})", known.join(", "))
                    });
                }
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(out)
}

/// `Entity.` at the start of a chain → the entity with the filters bound to it.
pub fn rewrite_scopes(src: &str, scopes: &[(String, String)]) -> String {
    if scopes.is_empty() {
        return src.to_string();
    }
    let ch: Vec<char> = src.chars().collect();
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(src.len() + 64);
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        if c == '"' || (c == 'f' && i + 1 < ch.len() && ch[i + 1] == '"' && (i == 0 || !ident(ch[i - 1]))) {
            let start = i;
            if c == 'f' {
                i += 1;
            }
            i += 1;
            while i < ch.len() && ch[i] != '"' {
                if ch[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(ch.len());
            out.extend(&ch[start..i]);
            continue;
        }
        if c == '#' {
            out.extend(&ch[i..]);
            break;
        }
        if c.is_ascii_alphabetic() && (i == 0 || !(ident(ch[i - 1]) || ch[i - 1] == '.')) {
            let start = i;
            while i < ch.len() && ident(ch[i]) {
                i += 1;
            }
            let word: String = ch[start..i].iter().collect();
            match scopes.iter().find(|(e, _)| *e == word) {
                Some((_, scoped)) if i < ch.len() && ch[i] == '.' => {
                    out.push_str(scoped);
                    i += 1;
                }
                _ => out.push_str(&word),
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> Param {
        Param { name: name.into(), var: format!("__p_{name}"), ty: None, value: V::None }
    }

    #[test]
    fn at_names() {
        let ps = vec![p("region"), p("amount_min")];
        assert_eq!(rewrite_params("Order.count(o => o.customer.region == @region) + @amount_min", &ps).unwrap(), "Order.count(o => o.customer.region == __p_region) + __p_amount_min");
        assert_eq!(rewrite_params("\"@region\" + x # @nope", &ps).unwrap(), "\"@region\" + x # @nope");
        assert!(rewrite_params("@nope", &ps).unwrap_err().contains("@region"));
    }

    #[test]
    fn scopes() {
        let s = vec![("Order".to_string(), "Order.where(__q => C).".to_string())];
        assert_eq!(rewrite_scopes("Order.count() + o.Order.x + \"Order.\" + Orders.count() + Order", &s), "Order.where(__q => C).count() + o.Order.x + \"Order.\" + Orders.count() + Order");
    }
}
