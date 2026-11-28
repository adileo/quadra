//! The HTTP API of Quadra without HTTP: what the model looks like to a user, queries, records
//! and the operations of a transaction, on an interpreter. The server runs them over the
//! database; the browser sandbox (crates/quadra-wasm) over its in-memory session. Same
//! JSON in both, so the web client works on either.

pub mod groups;
pub mod json;

use json::{cell, decode, decode_fields, describe, kind};
use quadra_ir::{EntityKind, Model, Span, Ty};
use quadra_runtime::{Flow, Interp, V};
use quadra_types::Report;
use serde_json::{json, Value as J};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// An answer that is not a success: (HTTP status, message).
pub type Fail = (u16, String);

/// Entities, fields, actions and flows the user of `i` may see, with what they may do.
pub fn model_json(
    i: &mut Interp,
    model: &Model,
    report: &Report,
    module_of: &dyn Fn(&Span) -> String,
) -> Vec<J> {
    let mut entities = Vec::new();
    for e in &model.entities {
        if e.temporary || e.kind != EntityKind::Entity || !i.can("read", &e.name, None, None) {
            continue;
        }
        let mut fields = Vec::new();
        for f in model.all_fields(e) {
            if i.hidden(&e.name, &f.name) {
                continue;
            }
            let t = report.field_types.get(&(e.name.clone(), f.name.clone()));
            let mut k = t.map(kind).unwrap_or(json!({ "kind": "other" }));
            // a value record (an address): edited as a small form of its fields
            if let Some(re) = k["entity"]
                .as_str()
                .and_then(|n| model.entity(n))
                .filter(|r| r.kind == EntityKind::Record)
            {
                let subs: Vec<J> = model
                    .all_fields(re)
                    .iter()
                    .filter(|sf| sf.formula.is_none())
                    .map(|sf| {
                        let mut sk = report
                            .field_types
                            .get(&(re.name.clone(), sf.name.clone()))
                            .map(kind)
                            .unwrap_or(json!({ "kind": "other" }));
                        sk["name"] = json!(sf.name);
                        sk["optional"] = json!(
                            matches!(sf.ty, Some(Ty::Optional(_)))
                                || sf.required_when.is_some()
                                || sf.default.is_some()
                        );
                        sk
                    })
                    .collect();
                k = json!({ "kind": "record", "entity": re.name, "fields": subs });
            }
            let optional = matches!(
                f.ty,
                Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_))
            );
            let o = k.as_object_mut().unwrap();
            o.insert("name".into(), json!(f.name));
            o.insert("formula".into(), json!(f.formula.is_some()));
            o.insert(
                "required".into(),
                json!(
                    f.formula.is_none()
                        && !optional
                        && f.default.is_none()
                        && f.required_when.is_none()
                        && !e.is_ledger
                ),
            );
            o.insert("has_default".into(), json!(f.default.is_some()));
            o.insert(
                "optional".into(),
                json!(matches!(f.ty, Some(Ty::Optional(_))) || f.required_when.is_some()),
            );
            o.insert(
                "editable".into(),
                json!(
                    f.formula.is_none()
                        && !e.is_ledger
                        && i.can("edit", &e.name, Some(&f.name), None)
                        && !e.flows.iter().any(|fl| fl.field == f.name)
                ),
            );
            o.insert("parent".into(), json!(f.parent));
            // a list of records: its own references (`members: [User]`), the lines that belong
            // to the record (`lines: [OrderLine]`, their `parent`), or the records that point
            // to it (`orders: [Order] via customer`)
            if let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty {
                if let Some(target) = entity_named(inner).and_then(|n| model.entity(&n)) {
                    let children = model.all_fields(target).iter().any(|tf| {
                        tf.parent
                            && tf.ty.as_ref().and_then(entity_named).as_deref()
                                == Some(e.name.as_str())
                    });
                    let link = if f.via.is_some() || f.through.is_some() {
                        "via"
                    } else if children {
                        "children"
                    } else {
                        "refs"
                    };
                    o.insert("link".into(), json!(link));
                    // the field of the other side holding this record: its lines are a query
                    // `refs: {back: id}` (paged)
                    if f.through.is_none() && quadra_store_is_inverse(model, e, f) {
                        let back = f.via.clone().or_else(|| {
                            target
                                .fields
                                .iter()
                                .find(|x| {
                                    x.ty.as_ref()
                                        .and_then(entity_named)
                                        .map_or(false, |n| n == e.name || e.traits.contains(&n))
                                })
                                .map(|x| x.name.clone())
                        });
                        if let Some(b) = back {
                            o.insert("back".into(), json!(b));
                        }
                    }
                }
            }
            o.insert("per_company".into(), json!(f.per_company));
            // a `File` field: the families and types it takes (none: any), its largest file
            if let Some(spec) = &f.file {
                let accept: Vec<&str> = spec.accept.iter().map(|(a, _)| a.as_str()).collect();
                o.insert(
                    "file".into(),
                    json!({ "accept": accept, "max": spec.max_bytes() }),
                );
            }
            fields.push(k);
        }
        let mut actions = Vec::new();
        for a in &e.actions {
            if !i.can("run", &e.name, Some(&a.name), None) {
                continue;
            }
            actions.push(json!({ "name": a.name, "params": action_params(a) }));
        }
        let flows: Vec<J> = e
            .flows
            .iter()
            .map(|fl| {
                let ts: Vec<J> = fl.transitions.iter().map(|t| json!({ "from": t.from, "to": t.to, "by": t.by, "automatic": t.by.is_none() })).collect();
                json!({ "field": fl.field, "transitions": ts })
            })
            .collect();
        for fl in &e.flows {
            for t in fl.transitions.iter().filter_map(|t| t.by.as_ref()) {
                if !actions.iter().any(|a| a["name"] == json!(t))
                    && i.can("run", &e.name, Some(t), None)
                {
                    actions.push(json!({ "name": t, "params": [] }));
                }
            }
        }
        entities.push(json!({
            "name": e.name,
            "module": module_of(&e.span),
            "ledger": e.is_ledger,
            "traits": e.traits,
            "can_create": !e.is_ledger && i.can("create", &e.name, None, None),
            // `deletable`: whether the caller may delete its records, and for how long they
            // stay in its trash (none: deleted for good)
            "can_delete": e.deletable.is_some() && i.can("delete", &e.name, None, None),
            "trash": e.deletable.as_ref().and_then(|d| d.trash.as_ref()).map(|t| format!("{} {}", t.n, t.unit)),
            "history_kept": match e.keep {
                quadra_ir::Keep::Full => match &e.history {
                    Some(t) => format!("{} {}", t.n, t.unit),
                    None => "full".to_string(),
                },
                quadra_ir::Keep::Last(n) => format!("last {n}"),
                quadra_ir::Keep::None => "none".to_string(),
            },
            "frozen_when": e.frozen.as_ref().map(|f| quadra_ir::print::expr(&f.when)),
            "fields": fields,
            "actions": actions,
            "flows": flows,
        }));
    }
    entities
}

/// The parameters of an action: name, type, its default (as written) and whether it may be
/// left empty (a parameter form shows them).
fn action_params(a: &quadra_ir::Action) -> Vec<J> {
    a.params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "type": p.ty.as_ref().map(quadra_ir::print::ty),
                "default": p.default.as_ref().map(quadra_ir::print::expr),
                "optional": p.default.is_some() || matches!(p.ty, Some(Ty::Optional(_))),
            })
        })
        .collect()
}

/// The formulas a query adds to its entity: the `where` filter and personal columns.
pub fn query_items(q: &J) -> Result<(String, Vec<(String, String)>, Vec<String>), Fail> {
    let entity = q["entity"]
        .as_str()
        .ok_or((400, "`entity` is required".to_string()))?
        .to_string();
    let mut items: Vec<(String, String)> = Vec::new();
    if let Some(w) = q["where"].as_str().filter(|w| !w.trim().is_empty()) {
        items.push(("__where".into(), w.to_string()));
    }
    let mut columns: Vec<String> = Vec::new();
    if let Some(cols) = q["columns"].as_object() {
        for (n, e) in cols {
            let ok = !n.is_empty()
                && n.len() <= 40
                && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err((
                    400,
                    format!("column name `{n}`: letters, digits and _ only"),
                ));
            }
            let e = e
                .as_str()
                .ok_or((400, format!("column `{n}`: an expression as text")))?;
            items.push((format!("__col_{n}"), e.to_string()));
            columns.push(n.clone());
        }
    }
    for (_, e) in &items {
        if e.contains('\n') || e.contains('\r') || e.len() > 2000 || e.trim().is_empty() {
            return Err((
                400,
                "an expression must be one line of at most 2000 characters".into(),
            ));
        }
    }
    Ok((entity, items, columns))
}

/// The source that adds the query's formulas: checked like any code (the checked expression
/// is then translated to parameterized SQL: SQL text is never made from what the user wrote).
pub fn overlay_source(entity: &str, items: &[(String, String)]) -> String {
    let mut src = format!("extend {entity}:\n");
    for (n, e) in items {
        src.push_str(&format!("    {n} = {e}\n"));
    }
    src
}

/// The only difference an overlay may make: these formulas on this entity.
pub fn same_shape(base: &Model, over: &Model, entity: &str, items: &[(String, String)]) -> bool {
    base.entities.len() == over.entities.len()
        && base.reactions.len() == over.reactions.len()
        && base.roles.len() == over.roles.len()
        && base.rules.len() == over.rules.len()
        && base
            .entities
            .iter()
            .zip(over.entities.iter())
            .all(|(a, b)| {
                let extra = if a.name == entity { items.len() } else { 0 };
                a.name == b.name
                    && a.actions.len() == b.actions.len()
                    && a.checks.len() == b.checks.len()
                    && a.posts.len() == b.posts.len()
                    && a.fields.len() + extra == b.fields.len()
            })
        && over.entity(entity).map_or(false, |e| {
            items
                .iter()
                .all(|(n, _)| e.fields.iter().any(|f| &f.name == n && f.formula.is_some()))
        })
}

/// A field path (`customer.name`) of a record, respecting hidden fields and read rules.
pub fn path_cell(i: &mut Interp, ent: &str, rec: &V, path: &str) -> J {
    let mut cur = rec.clone();
    let mut cur_ent = ent.to_string();
    let segs: Vec<&str> = path.split('.').collect();
    for (k, seg) in segs.iter().enumerate() {
        if i.hidden(&cur_ent, seg) {
            return json!({ "hidden": true });
        }
        let v = match i.get(&cur, seg, None, None) {
            Ok(v) => v,
            Err(e) => return json!({ "error": describe(e) }),
        };
        if k + 1 == segs.len() {
            return cell(i, &v);
        }
        match &v {
            V::Ref { ent, .. } => {
                if !i.can("read", ent, None, Some(&v)) {
                    return json!({ "hidden": true });
                }
                cur_ent = ent.to_string();
                cur = v;
            }
            V::None => return json!({ "v": null, "d": "" }),
            _ => return json!({ "error": format!("`{seg}` is not a record") }),
        }
    }
    json!(null)
}

/// How the grid orders a cell: numbers by value, other values by their text in ASCII lower
/// case (byte order), none (and what the user may not see) last. SQL orders the same way.
pub fn sort_key(v: &J) -> (u8, rust_decimal::Decimal, String) {
    let raw = &v["v"];
    let num = match raw {
        J::Number(n) => n.to_string().parse().ok(),
        J::Object(o) => o
            .get("money")
            .or_else(|| o.get("qty"))
            .or_else(|| o.get("d"))
            .or_else(|| o.get("pct"))
            .or_else(|| o.get("price"))
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok()),
        _ => None,
    };
    match (raw, num) {
        (J::Null, _) => (2, Default::default(), String::new()),
        (_, Some(n)) => (0, n, String::new()),
        _ => (
            1,
            Default::default(),
            v["d"].as_str().unwrap_or("").to_ascii_lowercase(),
        ),
    }
}

/// Rows counted exactly before a total becomes an estimate.
pub const COUNT_CAP: u64 = 10_000;

thread_local! {
    static CAP: std::cell::Cell<u64> = const { std::cell::Cell::new(COUNT_CAP) };
}

/// The cap of this thread (tests lower it to see estimates on small worlds).
pub fn set_count_cap(n: u64) {
    CAP.with(|c| c.set(n));
}

fn count_cap() -> u64 {
    CAP.with(|c| c.get())
}
/// Candidate rows the interpreter looks at for one page (row permissions, conditions SQL
/// cannot translate) before it answers with a shorter page and a cursor.
const SCAN_CAP: usize = 20_000;

/// An opaque cursor: where the next page starts.
fn encode_cursor(c: &J) -> String {
    c.to_string().bytes().map(|b| format!("{b:02x}")).collect()
}

fn decode_cursor(s: &str) -> Result<J, Fail> {
    let bad = || (400, "`after`: not a cursor of this query".to_string());
    if s.len() % 2 != 0 || s.len() > 4000 {
        return Err(bad());
    }
    let bytes: Result<Vec<u8>, _> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect();
    let text = String::from_utf8(bytes.map_err(|_| bad())?).map_err(|_| bad())?;
    serde_json::from_str(&text).map_err(|_| bad())
}

/// The order of a query: (field path, descending), at most 5 keys.
fn order_of(q: &J) -> Vec<(String, bool)> {
    q["order_by"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some((
                        o["field"].as_str()?.to_string(),
                        o["desc"].as_bool().unwrap_or(false),
                    ))
                })
                .take(5)
                .collect()
        })
        .unwrap_or_default()
}

/// The `where` of a query: the formula the overlay added.
fn where_expr<'m>(
    model: &'m Model,
    entity: &str,
    items: &[(String, String)],
) -> Option<&'m quadra_ir::Expr> {
    if items.first().map_or(true, |(n, _)| n != "__where") {
        return None;
    }
    model
        .entity(entity)?
        .fields
        .iter()
        .find(|f| f.name == "__where")?
        .formula
        .as_ref()
}

/// The lines of a record: `refs: {"order": 12}` (a reference, or a set holding the record).
fn refs_of(model: &Model, entity: &str, q: &J) -> Result<Vec<(String, u64)>, Fail> {
    let mut out = Vec::new();
    if let Some(o) = q["refs"].as_object() {
        let e = model
            .entity(entity)
            .ok_or((404, "unknown entity".to_string()))?;
        for (k, v) in o.iter().take(5) {
            let f = model
                .find_field(e, k)
                .ok_or((400, format!("`refs`: `{entity}` has no field `{k}`")))?;
            if f.formula.is_some() {
                return Err((400, format!("`refs`: `{k}` is a formula")));
            }
            let id = v
                .as_u64()
                .ok_or((400, format!("`refs`: `{k}` needs a record id")))?;
            out.push((k.clone(), id));
        }
    }
    Ok(out)
}

/// The grid's keys that SQL may order by: a field hidden on the path orders nothing (every
/// cell is hidden); a path through records whose reading depends on the row is the
/// interpreter's.
fn grid_keys(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    order: &[(String, bool)],
) -> Option<Vec<(Vec<String>, bool)>> {
    let mut out = Vec::new();
    'keys: for (path, desc) in order {
        let segs: Vec<String> = path.split('.').map(str::to_string).collect();
        let mut ent = entity.to_string();
        for (k, seg) in segs.iter().enumerate() {
            if i.hidden(&ent, seg) {
                continue 'keys;
            }
            if k + 1 == segs.len() {
                break;
            }
            let e = model.entity(&ent)?;
            let f = model.find_field(e, seg)?;
            let next = f.ty.as_ref().and_then(entity_named)?;
            if model
                .entity(&next)
                .map_or(true, |x| x.kind != EntityKind::Entity)
                || !i.read_unconditional(&next)
            {
                return None;
            }
            ent = next;
        }
        out.push((segs, *desc));
    }
    Some(out)
}

/// The value of an aggregate (`sum`, `count`, `min`, `max`, `avg` of a field path, none
/// skipped) over rows, as the interpreter computes it.
fn aggregate(i: &mut Interp, ent: &str, ids: &[u64], path: &str, func: &str) -> Result<V, String> {
    let mut vals = Vec::new();
    for id in ids {
        let mut cur = V::Ref {
            ent: Rc::from(ent),
            id: *id,
        };
        for seg in path.split('.') {
            cur = match &cur {
                V::None => V::None,
                _ => i.get(&cur, seg, None, None).map_err(describe)?,
            };
        }
        if !cur.is_none() {
            vals.push(cur);
        }
    }
    let fail = |e: Flow| describe(e);
    Ok(match func {
        "count" => V::Int(vals.len() as i64),
        "sum" | "avg" => {
            let mut acc = V::Int(0);
            for v in &vals {
                acc = i
                    .binop(quadra_ir::BinOp::Add, acc, v.clone())
                    .map_err(fail)?;
            }
            if func == "avg" {
                if vals.is_empty() {
                    V::None
                } else {
                    i.binop(quadra_ir::BinOp::Div, acc, V::Int(vals.len() as i64))
                        .map_err(fail)?
                }
            } else {
                acc
            }
        }
        _ => {
            let mut best: Option<V> = None;
            for v in vals {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let lt = i
                            .binop(quadra_ir::BinOp::Lt, v.clone(), b.clone())
                            .map_err(fail)?
                            .truthy();
                        if lt == (func == "min")
                            && i.binop(quadra_ir::BinOp::Ne, v.clone(), b.clone())
                                .map_err(fail)?
                                .truthy()
                        {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            best.unwrap_or(V::None)
        }
    })
}

/// The value of an aggregate from SQL's answer (None: the interpreter computes it).
fn aggregate_value(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    path: &str,
    func: &str,
    a: &quadra_runtime::query::AggOut,
) -> Option<V> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    let d = |s: &Option<String>| s.as_deref().and_then(|x| Decimal::from_str(x).ok());
    // the declared type of the last field: percentages and enums are not plain numbers and text
    let mut ent = entity.to_string();
    let mut last: Option<&quadra_ir::Field> = None;
    for seg in path.split('.') {
        let f = model.entity(&ent).and_then(|e| model.find_field(e, seg))?;
        if let Some(n) = f.ty.as_ref().and_then(entity_named) {
            ent = n;
        }
        last = Some(f);
    }
    let ty = last.and_then(|f| f.ty.clone());
    let named = |t: &Option<Ty>, n: &str| match t {
        Some(Ty::Path { segments, .. }) => segments.last().map(String::as_str) == Some(n),
        Some(Ty::Optional(x)) => {
            matches!(x.as_ref(), Ty::Path { segments, .. } if segments.last().map(String::as_str) == Some(n))
        }
        _ => false,
    };
    let pct = named(&ty, "Percent");
    let one = |s: &Option<String>| -> Option<V> {
        let x = s.as_ref()?;
        Some(match a.kind {
            "int" | "duration" if a.kind == "int" => V::Int(x.parse().ok()?),
            "duration" => V::Duration(x.parse().ok()?),
            "num" if pct => V::Pct(Decimal::from_str(x).ok()?),
            "num" => V::Dec(Decimal::from_str(x).ok()?),
            "money" => V::Money {
                amt: Decimal::from_str(x).ok()?,
                cur: Rc::from(a.unit.as_deref()?),
            },
            "qty" => V::Qty {
                v: Decimal::from_str(x).ok()?,
                unit: Rc::from(a.unit.as_deref()?),
            },
            "date" => V::Date(chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").ok()?),
            "datetime" => V::DateTime(
                chrono::NaiveDateTime::parse_from_str(&x[..x.len().min(19)], "%Y-%m-%d %H:%M:%S")
                    .ok()?,
            ),
            _ if matches!(ty.as_ref().map(|t| matches!(t, Ty::Enum(_))), Some(true)) => {
                V::Variant {
                    name: Rc::from(x.as_str()),
                    fields: Rc::new(Vec::new()),
                }
            }
            _ => V::Text(Rc::from(x.as_str())),
        })
    };
    if matches!(a.kind, "money" | "qty") && a.units > 1 {
        return None;
    }
    Some(match func {
        "count" => V::Int(a.rows as i64),
        "sum" | "avg" => {
            let sum = if a.rows == 0 {
                V::Int(0)
            } else {
                match a.kind {
                    "int" => V::Int(a.value.as_deref()?.parse().ok()?),
                    "duration" => V::Duration(a.value.as_deref()?.parse().ok()?),
                    "num" => V::Dec(d(&a.value)?),
                    "money" | "qty" => one(&a.value)?,
                    _ => return None,
                }
            };
            if func == "sum" {
                sum
            } else if a.rows == 0 {
                V::None
            } else {
                i.binop(quadra_ir::BinOp::Div, sum, V::Int(a.rows as i64))
                    .ok()?
            }
        }
        _ => {
            if a.rows == 0 {
                V::None
            } else {
                one(&a.value)?
            }
        }
    })
}

/// Rows of a query (the model must include the overlay of `items`).
///
/// - `where` (a condition on the record's current values), `order_by` (field paths), `limit`
///   (at most 1000) with `offset` or `after` (the `next` cursor of the previous page);
/// - `total` (exact up to `COUNT_CAP` rows, else `estimated: true`; `"count": false` skips it);
/// - `aggregates: {"field": "sum" | "count" | "min" | "max" | "avg"}` of the filtered rows;
/// - `ids` (a selection) and `refs` (`{"order": 12}`: the lines of a record).
///
/// On a database the page is read in SQL when the query translates (the interpreter decides
/// the rows SQL flags, the conditions it cannot translate and the row permissions); in memory
/// every record is evaluated.
pub fn query_rows(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    q: &J,
    items: &[(String, String)],
    columns: &[String],
) -> Result<J, Fail> {
    if !q["group_by"].is_null() {
        return groups::group_rows(i, model, entity, q, items);
    }
    let entity = entity.to_string();
    if !i.can("read", &entity, None, None) {
        return Err((403, format!("permission denied: cannot read {entity}")));
    }
    let fields: Vec<String> = match q["fields"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .take(100)
            .collect(),
        None => {
            let e = model.entity(&entity).unwrap();
            let names: Vec<String> = model
                .all_fields(e)
                .iter()
                .filter(|f| {
                    !matches!(f.ty, Some(Ty::List(_)) | Some(Ty::Set(_)))
                        && !f.name.starts_with("__")
                })
                .map(|f| f.name.clone())
                .collect();
            names
                .into_iter()
                .filter(|n| !i.hidden(&entity, n))
                .collect()
        }
    };
    let order = order_of(q);
    let offset = q["offset"].as_u64().unwrap_or(0) as usize;
    let limit = q["limit"].as_u64().unwrap_or(200).min(1000) as usize;
    let after = match q["after"].as_str() {
        Some(c) if !c.is_empty() => Some(decode_cursor(c)?),
        _ => None,
    };
    let want_count = q["count"].as_bool().unwrap_or(true);
    let aggs: Vec<(String, String)> = match q["aggregates"].as_object() {
        Some(o) => o
            .iter()
            .take(20)
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect(),
        None => Vec::new(),
    };
    for (f, func) in &aggs {
        if !matches!(func.as_str(), "sum" | "count" | "min" | "max" | "avg") {
            return Err((
                400,
                format!("aggregate of `{f}`: sum, count, min, max or avg"),
            ));
        }
    }
    let refs = refs_of(model, &entity, q)?;
    let wexpr = where_expr(model, &entity, items);
    let ids: Option<Vec<u64>> = q["ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).take(1000).collect());
    let filter = groups::group_filter(i, model, &entity, q)?;
    let search = groups::search_of(i, model, &entity, q)?;
    let flt = groups::filters_of(i, model, &entity, q)?;

    let this = quadra_runtime::query::Binder::This;
    let conjuncts: Vec<&quadra_ir::Expr> = wexpr
        .map(quadra_runtime::query::conjuncts)
        .unwrap_or_default();
    let mut spec = quadra_runtime::Spec::new(&entity);
    spec.read_check = true;
    spec.ids = ids.clone();
    spec.refs = refs.clone();
    spec.keys = filter.clone();
    spec.search = search.clone();
    spec.conds = conjuncts
        .iter()
        .map(|e| quadra_runtime::query::Cond {
            binder: &this,
            expr: e,
            negate: false,
        })
        .collect();
    spec.conds.extend(flt.conds());

    // ---- the page: SQL when the query translates ----
    let mut page_ids: Option<(Vec<u64>, Option<J>)> = None;
    let mut pass_ratio = 1.0;
    let sql_cursor = match &after {
        Some(c) if c["k"] == json!("s") => Some(c),
        Some(_) => None,
        None => None,
    };
    let keys = grid_keys(i, model, &entity, &order);
    if let (Some(keys), true) = (&keys, after.as_ref().map_or(true, |c| c["k"] == json!("s"))) {
        spec.order = keys
            .iter()
            .map(|(p, d)| quadra_runtime::query::Key::Grid {
                path: p.clone(),
                desc: *d,
            })
            .collect();
        // ties by id, in the direction of the first key
        spec.id_desc = order.first().map_or(false, |(_, d)| *d);
        let start = match sql_cursor {
            Some(c) => {
                let ks: Vec<Option<String>> = c["keys"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                Some(quadra_runtime::query::After {
                    keys: ks,
                    id: c["id"]
                        .as_u64()
                        .ok_or((400, "`after`: not a cursor of this query".to_string()))?,
                })
            }
            None => None,
        };
        let mut ctx = flt.ctx();
        let skip = if start.is_some() { 0 } else { offset };
        match i.sql_page(&spec, &mut ctx, start, skip, limit, SCAN_CAP) {
            Ok(Some(p)) => {
                if p.scanned > 0 {
                    pass_ratio = p.kept as f64 / p.scanned as f64;
                }
                let next = p
                    .next
                    .map(|a| json!({ "k": "s", "keys": a.keys, "id": a.id }));
                page_ids = Some((p.ids, next));
            }
            Ok(None) => {}
            Err(e) => return Err((422, format!("where: {}", describe(e)))),
        }
    }
    let sql_paged = page_ids.is_some();

    // ---- the interpreter's way: every record ----
    let mut matched_all: Option<Vec<u64>> = None;
    if page_ids.is_none() {
        let all = match &ids {
            Some(a) => a.clone(),
            None => i.ids(&entity, None),
        };
        let mut matched: Vec<V> = Vec::new();
        for id in all {
            let rec = V::Ref {
                ent: Rc::from(entity.as_str()),
                id,
            };
            if !i.store.exists(id, None)
                || i.store.get(id).map_or(true, |r| *r.ent != *entity)
                || !i.can("read", &entity, None, Some(&rec))
            {
                continue;
            }
            let mut ok = true;
            for (f, target) in &refs {
                let v = i.store.read(id, f, None);
                if !quadra_runtime::store::same(
                    v.as_ref(),
                    &V::Ref {
                        ent: Rc::from(""),
                        id: *target,
                    },
                ) {
                    ok = false;
                }
            }
            if !ok || !groups::in_scope(i, &entity, &rec, &filter, search.as_ref())? {
                continue;
            }
            if !flt.holds(i, &rec)? {
                continue;
            }
            if let Some(w) = wexpr {
                match i.eval_on(&rec, &entity, w) {
                    Ok(v) if v.truthy() => {}
                    Ok(_) => continue,
                    Err(e) => return Err((422, format!("where: {}", describe(e)))),
                }
            }
            matched.push(rec);
        }
        if !order.is_empty() {
            let mut keyed: Vec<(Vec<(u8, rust_decimal::Decimal, String)>, V)> = Vec::new();
            for rec in matched {
                let k = order
                    .iter()
                    .map(|(p, _)| sort_key(&path_cell(i, &entity, &rec, p)))
                    .collect();
                keyed.push((k, rec));
            }
            // ties by id, in the direction of the first key (an index on the key and the id
            // gives the page in this order)
            let first_desc = order.first().map_or(false, |(_, d)| *d);
            keyed.sort_by(|a, b| {
                for (n, (_, desc)) in order.iter().enumerate() {
                    let o = a.0[n].cmp(&b.0[n]);
                    let o = if *desc { o.reverse() } else { o };
                    if o != std::cmp::Ordering::Equal {
                        return o;
                    }
                }
                let (x, y) = (a.1.as_ref_id().unwrap_or(0), b.1.as_ref_id().unwrap_or(0));
                if first_desc {
                    y.cmp(&x)
                } else {
                    x.cmp(&y)
                }
            });
            matched = keyed.into_iter().map(|(_, r)| r).collect();
        }
        let all_ids: Vec<u64> = matched.iter().filter_map(|r| r.as_ref_id()).collect();
        let start = match &after {
            Some(c) => c["o"]
                .as_u64()
                .ok_or((400, "`after`: not a cursor of this query".to_string()))?
                as usize,
            None => offset,
        };
        let page: Vec<u64> = all_ids.iter().skip(start).take(limit).copied().collect();
        let next = (start + page.len() < all_ids.len())
            .then(|| json!({ "k": "o", "o": start + page.len() }));
        page_ids = Some((page, next));
        matched_all = Some(all_ids);
    }
    let (page, next) = page_ids.unwrap();

    // ---- total ----
    let mut total: Option<(u64, bool)> = None;
    if want_count {
        total = match &matched_all {
            Some(all) => Some((all.len() as u64, false)),
            None => {
                let mut ctx = flt.ctx();
                let mut cspec = spec.clone();
                cspec.order = Vec::new();
                match i.sql_count(&cspec, &mut ctx, Some(count_cap())) {
                    Ok(Some((n, true))) => Some((
                        ((n as f64)
                            * if i.read_unconditional(&entity) {
                                1.0
                            } else {
                                pass_ratio
                            })
                        .round() as u64,
                        true,
                    )),
                    Ok(Some(x)) => Some(x),
                    Ok(None) => None,
                    Err(e) => return Err((422, format!("where: {}", describe(e)))),
                }
            }
        };
        if total.is_none() {
            // the data changed under SQL: count the interpreter's way
            let mut n = 0u64;
            for id in i.ids(&entity, None) {
                let rec = V::Ref {
                    ent: Rc::from(entity.as_str()),
                    id,
                };
                if !i.can("read", &entity, None, Some(&rec)) {
                    continue;
                }
                if refs.iter().any(|(f, t)| {
                    !quadra_runtime::store::same(
                        i.store.read(id, f, None).as_ref(),
                        &V::Ref {
                            ent: Rc::from(""),
                            id: *t,
                        },
                    )
                }) {
                    continue;
                }
                if let Some(sel) = &ids {
                    if !sel.contains(&id) {
                        continue;
                    }
                }
                if !groups::in_scope(i, &entity, &rec, &filter, search.as_ref())? {
                    continue;
                }
                if !flt.holds(i, &rec)? {
                    continue;
                }
                if let Some(w) = wexpr {
                    if !i
                        .eval_on(&rec, &entity, w)
                        .map_err(|e| (422, format!("where: {}", describe(e))))?
                        .truthy()
                    {
                        continue;
                    }
                }
                n += 1;
            }
            total = Some((n, false));
        }
    }

    // ---- aggregates ----
    let mut agg_out = serde_json::Map::new();
    let agg_fn = |func: &str| match func {
        "sum" | "avg" => quadra_runtime::query::AggFn::Sum,
        "min" => quadra_runtime::query::AggFn::Min,
        "max" => quadra_runtime::query::AggFn::Max,
        _ => quadra_runtime::query::AggFn::Count,
    };
    // every aggregate in one query when they all translate (else one by one below)
    let mut together: HashMap<String, quadra_runtime::query::AggOut> = HashMap::new();
    if sql_paged && aggs.len() > 1 {
        let list: Vec<quadra_runtime::query::Agg> = aggs
            .iter()
            .map(|(path, func)| quadra_runtime::query::Agg {
                of: quadra_runtime::query::AggOf::Path(
                    path.split('.').map(str::to_string).collect(),
                ),
                func: agg_fn(func),
            })
            .collect();
        let mut ctx = flt.ctx();
        let mut aspec = spec.clone();
        aspec.order = Vec::new();
        if let Ok(Some((_, outs))) = i.sql_aggs(&aspec, &mut ctx, &list) {
            for ((path, func), out) in aggs.iter().zip(outs) {
                together.insert(format!("{path}|{func}"), out);
            }
        }
    }
    for (path, func) in &aggs {
        let segs: Vec<String> = path.split('.').map(str::to_string).collect();
        let hidden = {
            let mut ent = entity.clone();
            let mut h = false;
            for seg in &segs {
                if i.hidden(&ent, seg) {
                    h = true;
                }
                if let Some(n) = model
                    .entity(&ent)
                    .and_then(|e| model.find_field(e, seg))
                    .and_then(|f| f.ty.as_ref().and_then(entity_named))
                {
                    ent = n;
                }
            }
            h
        };
        if hidden {
            agg_out.insert(path.clone(), json!({ "fn": func, "hidden": true }));
            continue;
        }
        let mut value: Option<Result<V, String>> = None;
        if let Some(out) = together.get(&format!("{path}|{func}")) {
            value = aggregate_value(i, model, &entity, path, func, out).map(Ok);
        }
        if sql_paged && value.is_none() && together.is_empty() {
            let a = quadra_runtime::query::Agg {
                of: quadra_runtime::query::AggOf::Path(segs.clone()),
                func: agg_fn(func),
            };
            let mut ctx = flt.ctx();
            let mut aspec = spec.clone();
            aspec.order = Vec::new();
            if let Ok(Some((_, outs))) = i.sql_aggs(&aspec, &mut ctx, &[a]) {
                value = aggregate_value(i, model, &entity, path, func, &outs[0]).map(Ok);
            }
        }
        if value.is_none() {
            // the interpreter's way, on every matching row (when there are not too many)
            let all = match &matched_all {
                Some(a) => Some(a.clone()),
                None => {
                    let mut ctx = flt.ctx();
                    let mut aspec = spec.clone();
                    aspec.order = Vec::new();
                    match i.sql_page(
                        &aspec,
                        &mut ctx,
                        None,
                        0,
                        COUNT_CAP as usize + 1,
                        usize::MAX,
                    ) {
                        Ok(Some(p)) if p.ids.len() <= COUNT_CAP as usize => Some(p.ids),
                        Ok(_) => None,
                        Err(e) => return Err((422, format!("where: {}", describe(e)))),
                    }
                }
            };
            value = Some(match all {
                Some(ids) => aggregate(i, &entity, &ids, path, func),
                None => Err(format!("more than {COUNT_CAP} rows to aggregate this way")),
            });
        }
        let cellv = match value.unwrap() {
            Ok(v) => {
                // canonical decimals (the database computes them from canonical decimals)
                let v = v.normalized();
                let mut c = cell(i, &v);
                c["fn"] = json!(func);
                c
            }
            Err(e) => json!({ "fn": func, "error": e }),
        };
        agg_out.insert(path.clone(), cellv);
    }

    // ---- the rows: the page's records and what their cells show, loaded in batches ----
    i.store.prefetch_as(&entity, &page);
    prefetch_paths(i, model, &entity, &page, &fields);
    let mut rows = Vec::new();
    for id in page {
        let rec = V::Ref {
            ent: Rc::from(entity.as_str()),
            id,
        };
        let mut cells = serde_json::Map::new();
        for f in &fields {
            cells.insert(f.clone(), path_cell(i, &entity, &rec, f));
        }
        for c in columns {
            let v = match i.get(&rec, &format!("__col_{c}"), None, None) {
                Ok(v) => cell(i, &v),
                Err(e) => json!({ "error": describe(e) }),
            };
            cells.insert(c.clone(), v);
        }
        let (version, warnings) = i
            .store
            .get(id)
            .map(|r| (r.version(), r.warnings.clone()))
            .unwrap_or_default();
        let frozen = i.frozen_owner(id).is_some();
        rows.push(json!({ "id": id, "version": version, "frozen": frozen, "warnings": warnings, "display": i.display(&rec), "cells": cells }));
    }
    let mut out = json!({
        "entity": entity, "base": i.store.base, "offset": offset, "rows": rows, "fields": fields, "columns": columns,
        "next": next.map(|c| encode_cursor(&c)),
    });
    if let Some((n, est)) = total {
        out["total"] = json!(n);
        out["estimated"] = json!(est);
    }
    if !aggs.is_empty() {
        out["aggregates"] = J::Object(agg_out);
    }
    Ok(out)
}

/// The records of the inverse collections of `ids` (records of `e`) that `wanted` names, in one
/// query per collection; and theirs, when the children have formulas (which may read them).
fn prefetch_lines(
    i: &mut Interp,
    model: &Model,
    e: &quadra_ir::Entity,
    ids: &[u64],
    wanted: &dyn Fn(&str) -> bool,
    depth: usize,
) {
    if ids.is_empty() || depth > 2 {
        return;
    }
    for f in model.all_fields(e) {
        if f.through.is_some() || !quadra_store_is_inverse(model, e, f) || !wanted(&f.name) {
            continue;
        }
        let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty else {
            continue;
        };
        let Some(child) = entity_named(inner).and_then(|n| model.entity(&n)) else {
            continue;
        };
        if child.kind != EntityKind::Entity {
            continue;
        }
        let back = f.via.clone().or_else(|| {
            child
                .fields
                .iter()
                .find(|x| {
                    x.ty.as_ref()
                        .and_then(entity_named)
                        .map_or(false, |n| n == e.name || e.traits.contains(&n))
                })
                .map(|x| x.name.clone())
        });
        let Some(b) = back else { continue };
        let found = i.store.prefetch_find(&child.name, &b, ids);
        // (a page's lines, not a whole subtree: a region's customers are read on demand); the
        // lines whose formulas are computed (not read from their stored values)
        let computed = computing(i, model, child, &found, None);
        if computed.len() <= 5_000 && !computed.is_empty() {
            prefetch_lines(i, model, child, &computed, &|_| true, depth + 1);
        }
    }
}

/// The records among `ids` (of `e`, loaded) on which a formula (of those `fields` names, all
/// when None) is computed when read: not an incremental formula with its stored value.
fn computing(
    i: &mut Interp,
    model: &Model,
    e: &quadra_ir::Entity,
    ids: &[u64],
    fields: Option<&[String]>,
) -> Vec<u64> {
    let formulas: Vec<String> = model
        .all_fields(e)
        .iter()
        .filter(|f| {
            f.formula.is_some() && fields.map_or(true, |fs| fs.iter().any(|x| x == &f.name))
        })
        .map(|f| f.name.clone())
        .collect();
    if formulas.is_empty() {
        return Vec::new();
    }
    ids.iter()
        .copied()
        .filter(|id| formulas.iter().any(|f| !i.has_stored(&e.name, *id, f)))
        .collect()
}

/// Load, in a few batches, what the cells of a page read: the records the shown paths go
/// through, and the documents of lines (their frozen state).
fn prefetch_paths(i: &mut Interp, model: &Model, entity: &str, page: &[u64], fields: &[String]) {
    // the lines of each record (its rollups read them): one query per collection, and the
    // lines of those lines when their own formulas read them (`Order.to_ship` → its lines'
    // shipments)
    if let Some(e) = model.entity(entity) {
        // the collection is shown (on every row), or a formula shown may read it (on the rows
        // where it is computed: not read from its stored value)
        let shown = |n: &str| {
            fields
                .iter()
                .any(|f| f == n || f.starts_with(&format!("{n}.")))
        };
        prefetch_lines(i, model, e, page, &shown, 0);
        let computed = computing(i, model, e, page, Some(fields));
        if !computed.is_empty() {
            prefetch_lines(i, model, e, &computed, &|n| !shown(n), 0);
        }
    }
    let mut level: Vec<(String, Vec<u64>)> = vec![(entity.to_string(), page.to_vec())];
    let mut paths: Vec<Vec<String>> = fields
        .iter()
        .map(|f| f.split('.').map(str::to_string).collect())
        .collect();
    // the shown references (their display) and the parent chain (frozen documents)
    if let Some(e) = model.entity(entity) {
        for f in model.all_fields(e) {
            if f.parent {
                paths.push(vec![f.name.clone(), "*".into()]);
            }
        }
    }
    for depth in 0..4 {
        let mut next: Vec<(String, Vec<u64>)> = Vec::new();
        for (ent, ids) in &level {
            let Some(e) = model.entity(ent) else { continue };
            let mut fields_here: Vec<&quadra_ir::Field> = Vec::new();
            for p in &paths {
                if let Some(seg) = p.get(depth) {
                    if let Some(f) = model.find_field(e, seg) {
                        if f.formula.is_none()
                            && entity_named(f.ty.as_ref().unwrap_or(&Ty::Error)).is_some()
                            && !fields_here.iter().any(|x| x.name == f.name)
                        {
                            fields_here.push(f);
                        }
                    }
                }
            }
            if depth > 0 {
                if let Some(pf) = model.all_fields(e).into_iter().find(|f| f.parent) {
                    if !fields_here.iter().any(|x| x.name == pf.name) {
                        fields_here.push(pf);
                    }
                }
            }
            for f in fields_here {
                let target = entity_named(f.ty.as_ref().unwrap()).unwrap_or_default();
                let mut t: Vec<u64> = ids
                    .iter()
                    .filter_map(|id| i.store.read(*id, &f.name, None).and_then(|v| v.as_ref_id()))
                    .collect();
                t.sort();
                t.dedup();
                if !t.is_empty() {
                    next.push((target, t));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        for (target, ids) in &next {
            i.store.prefetch_as(target, ids);
        }
        level = next;
    }
}

/// `GET /v1/search/{Entity}?q=&limit=&after=`: records found by what they are shown by
/// (`number`, `code`, `name`, `login`: the stored text fields among them the user may see)
/// starting with the text (ASCII case-insensitive) first, then those matching the full-text
/// search (every word of the text begins a word of a stored text field the user may see:
/// `query::searchable`), each group by the display text and id; only records the user may
/// read. `{id, name}` rows and the cursor of the next page.
pub fn search_rows(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    text: &str,
    limit: usize,
    after: Option<&str>,
) -> Result<J, Fail> {
    let e = model
        .entity(entity)
        .filter(|e| e.kind == EntityKind::Entity && !e.temporary)
        .ok_or((404, format!("unknown entity `{entity}`")))?;
    if !i.can("read", entity, None, None) {
        return Err((403, format!("permission denied: cannot read {entity}")));
    }
    let limit = limit.clamp(1, 200);
    let needle = text.trim().to_ascii_lowercase();
    if needle.chars().count() > 100 {
        return Err((400, "`q`: at most 100 characters".into()));
    }
    let fields: Vec<String> = quadra_store_display_fields()
        .iter()
        .filter_map(|n| model.find_field(e, n))
        .filter(|f| f.formula.is_none() && text_field(f))
        .map(|f| f.name.clone())
        .filter(|n| !i.hidden(entity, n))
        .collect();
    let start = match after {
        Some(c) if !c.is_empty() => {
            let c = decode_cursor(c)?;
            let keys: Vec<Option<String>> = c["keys"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            Some(quadra_runtime::query::After {
                keys,
                id: c["id"]
                    .as_u64()
                    .ok_or((400, "`after`: not a cursor of this search".to_string()))?,
            })
        }
        _ => None,
    };
    // then the full-text search: every word begins a word of a stored text field the user may see
    let words = quadra_runtime::query::TextSearch {
        words: quadra_runtime::query::search_words(text),
        fields: quadra_runtime::query::searchable(model, entity)
            .into_iter()
            .filter(|f| !i.hidden(entity, f))
            .collect(),
    };
    let permission = !i.read_unconditional(entity);
    // the row rules, applied in SQL when they translate
    let perm = if permission {
        i.perm_of("read", entity)
    } else {
        None
    };
    let mut frees = quadra_runtime::query::Frees::default();
    if let Some(p) = &perm {
        i.perm_frees(p, &mut frees);
    }
    let mut out: Vec<J> = Vec::new();
    let mut next: Option<quadra_runtime::query::After> = None;
    let src = i.store.src.clone();
    let mut sql_ok = src.is_some();
    if let Some(src) = src {
        let mut cursor = start.clone();
        let mut scanned = 0usize;
        // every row checked here (the rules did not translate): known from the first answer
        let mut each = false;
        'pages: loop {
            let batch = if each { 200 } else { limit + 1 };
            let found = src.borrow_mut().search(
                entity,
                &fields,
                &needle,
                Some(&words),
                cursor.as_ref(),
                batch,
                perm.as_ref().map(|p| (model, p, &frees)),
            );
            let found = match found {
                Ok(r) => r,
                Err(_) => {
                    sql_ok = false;
                    break;
                }
            };
            each = permission && !found.perm_sql;
            let rows = found.rows;
            let n = rows.len();
            let ids: Vec<u64> = rows.iter().map(|(id, _, _)| *id).collect();
            i.store.prefetch_as(&entity, &ids);
            for (id, c, check) in rows {
                scanned += 1;
                let rec = V::Ref {
                    ent: Rc::from(entity),
                    id,
                };
                if permission && (each || check) && !i.can("read", entity, None, Some(&rec)) {
                    cursor = Some(c);
                    continue;
                }
                if out.len() == limit {
                    next = cursor.clone();
                    break 'pages;
                }
                out.push(json!({ "id": id, "name": i.display(&rec) }));
                cursor = Some(c);
            }
            if n < batch {
                break;
            }
            if scanned >= 20_000 {
                next = cursor;
                break;
            }
        }
    }
    if !sql_ok {
        // in memory: the same order, every record
        out.clear();
        next = None;
        let mut found: Vec<(String, String, u64)> = Vec::new();
        for id in i.ids(entity, None) {
            let rec = V::Ref {
                ent: Rc::from(entity),
                id,
            };
            if permission && !i.can("read", entity, None, Some(&rec)) {
                continue;
            }
            let vals: Vec<String> = fields
                .iter()
                .filter_map(|f| i.store.read(id, f, None))
                .filter(|v| !v.is_none())
                .map(|v| v.to_string().to_ascii_lowercase())
                .collect();
            // (as SQL: an entity without text to search by lists every record)
            let rec_v = V::Ref {
                ent: Rc::from(entity),
                id,
            };
            let phase = if needle.is_empty()
                || fields.is_empty()
                || vals.iter().any(|v| v.starts_with(&needle))
            {
                "1"
            } else if i.searched(&rec_v, &words) {
                "2"
            } else {
                continue;
            };
            let key = quadra_store_display_fields()
                .iter()
                .find_map(|k| {
                    i.store
                        .read(id, k, None)
                        .filter(|v| !v.is_none())
                        .map(|v| v.to_string().to_ascii_lowercase())
                })
                .unwrap_or_default();
            found.push((phase.to_string(), key, id));
        }
        found.sort();
        let from = match &start {
            Some(a) => {
                let k = (
                    a.keys.first().cloned().flatten().unwrap_or_default(),
                    a.keys.get(1).cloned().flatten().unwrap_or_default(),
                    a.id,
                );
                found.iter().position(|x| *x > k).unwrap_or(found.len())
            }
            None => 0,
        };
        for (ph, key, id) in found.iter().skip(from).take(limit) {
            let rec = V::Ref {
                ent: Rc::from(entity),
                id: *id,
            };
            out.push(json!({ "id": id, "name": i.display(&rec) }));
            next = Some(quadra_runtime::query::After {
                keys: vec![Some(ph.clone()), Some(key.clone())],
                id: *id,
            });
        }
        if from + limit >= found.len() {
            next = None;
        }
    }
    Ok(
        json!({ "entity": entity, "rows": out, "next": next.map(|a| encode_cursor(&json!({ "keys": a.keys, "id": a.id }))) }),
    )
}

fn quadra_store_display_fields() -> &'static [&'static str] {
    &["number", "code", "name", "login"]
}

/// A stored field holding text (`Text`, `Code`…), optional or not.
fn text_field(f: &quadra_ir::Field) -> bool {
    let t = match &f.ty {
        Some(Ty::Optional(t)) => Some(t.as_ref()),
        t => t.as_ref(),
    };
    matches!(t, Some(Ty::Path { segments, args, .. }) if args.is_empty() && matches!(segments.last().map(String::as_str), Some("Text" | "Code" | "Email" | "Phone" | "VatNumber" | "Barcode")))
}

/// A record: every field, what may be edited now, the actions allowed now.
pub fn record_json(i: &mut Interp, model: &Model, entity: &str, id: u64) -> Result<J, Fail> {
    i.store.load_as(id, entity);
    let actual = i
        .store
        .get(id)
        .map(|r| r.ent.to_string())
        .ok_or((404, format!("{entity}#{id} not found")))?;
    let e = model
        .entity(&actual)
        .ok_or((404, "not found".to_string()))?;
    if actual != entity && !model.implements(e, entity) {
        return Err((404, format!("{entity}#{id} not found")));
    }
    let rec = V::Ref {
        ent: Rc::from(actual.as_str()),
        id,
    };
    if !i.can("read", &actual, None, Some(&rec)) {
        return Err((403, format!("permission denied: cannot read {actual}#{id}")));
    }
    let (version, warnings, cause) = {
        let r = i.store.get(id).unwrap();
        (r.version(), r.warnings.clone(), r.cause.chain())
    };
    // a line of a frozen document is frozen with it (and has no `except` of its own)
    let owner = i.frozen_owner(id);
    let frozen = owner.is_some();
    let mut cells = serde_json::Map::new();
    let mut editable = Vec::new();
    for f in model.all_fields(e) {
        if i.hidden(&actual, &f.name) {
            continue;
        }
        let v = match lines_of(model, e, f) {
            // the lines of the record: a page and their count, not every one of them
            Some((child, back)) => lines_cell(i, model, &rec, &f.name, &child, &back),
            None => match i.get(&rec, &f.name, None, None) {
                Ok(v) => cell(i, &v),
                Err(err) => json!({ "error": describe(err) }),
            },
        };
        cells.insert(f.name.clone(), v);
        let in_flow = e.flows.iter().any(|fl| fl.field == f.name);
        let frozen_ok = match owner {
            None => true,
            Some((o, _)) => {
                o == id
                    && e.frozen
                        .as_ref()
                        .map_or(false, |fr| fr.except.iter().any(|x| x == &f.name))
            }
        };
        if f.formula.is_none()
            && !in_flow
            && frozen_ok
            && i.can("edit", &actual, Some(&f.name), Some(&rec))
        {
            editable.push(f.name.clone());
        }
    }
    // actions available now: permission, and for flow transitions the current state
    let mut actions = Vec::new();
    for a in &e.actions {
        if i.can("run", &actual, Some(&a.name), Some(&rec)) {
            actions.push(json!({ "name": a.name, "params": action_params(a) }));
        }
    }
    for fl in &e.flows {
        let state = i
            .get(&rec, &fl.field, None, None)
            .map(|v| v.to_string())
            .unwrap_or_default();
        for t in &fl.transitions {
            let Some(by) = &t.by else { continue };
            if t.from.iter().any(|f| *f == state) && i.can("run", &actual, Some(by), Some(&rec)) {
                if let Some(a) = actions.iter_mut().find(|a| a["name"] == json!(by)) {
                    a["to"] = json!(t.to);
                } else {
                    actions.push(json!({ "name": by, "params": [], "to": t.to }));
                }
            }
        }
        // actions tied to a flow but not allowed from this state
        actions.retain(|a| {
            let name = a["name"].as_str().unwrap_or("");
            let tied = fl.transitions.iter().any(|t| t.by.as_deref() == Some(name));
            !tied
                || fl
                    .transitions
                    .iter()
                    .any(|t| t.by.as_deref() == Some(name) && t.from.iter().any(|f| *f == state))
        });
    }
    // the others, with the reason (a button of an app shows it)
    let mut unavailable: Vec<J> = Vec::new();
    let mut names: Vec<&str> = e.actions.iter().map(|a| a.name.as_str()).collect();
    for fl in &e.flows {
        for t in &fl.transitions {
            if let Some(b) = t.by.as_deref().filter(|b| !names.contains(b)) {
                names.push(b);
            }
        }
    }
    for name in names {
        if actions.iter().any(|a| a["name"] == json!(name)) {
            continue;
        }
        let reason = match i.check_permission("run", &actual, Some(name), Some(&rec)) {
            Err(f) => describe(f),
            Ok(()) => {
                let mut why = String::from("not available now");
                for fl in &e.flows {
                    let from: Vec<&str> = fl
                        .transitions
                        .iter()
                        .filter(|t| t.by.as_deref() == Some(name))
                        .flat_map(|t| t.from.iter().map(String::as_str))
                        .collect();
                    if !from.is_empty() {
                        let state = i
                            .get(&rec, &fl.field, None, None)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        why = format!("only from {} (it is {state})", from.join(" or "));
                    }
                }
                why
            }
        };
        unavailable.push(json!({ "name": name, "reason": reason }));
    }
    let display = i.display(&rec);
    Ok(json!({
        "entity": actual, "id": id, "display": display, "version": version, "base": i.store.base,
        "frozen": frozen, "warnings": warnings, "cause": cause, "cells": cells,
        "editable": editable, "actions": actions, "unavailable": unavailable,
        "history": i.can_read_history(&actual),
    }))
}

/// The records of a record's collection in the record JSON (`GET /v1/records`): the first
/// ones, then a cursor for `POST /v1/query` with `refs`.
pub const LINES_PAGE: usize = 50;

/// A collection computed from the other side (`customers: [Customer]`, the lines of a
/// document, `customers: {Customer} via tags`): (the entity of its records, the field of theirs
/// that holds the record). Its records are a query (`refs: {back: id}`).
fn lines_of(
    model: &Model,
    e: &quadra_ir::Entity,
    f: &quadra_ir::Field,
) -> Option<(String, String)> {
    if f.formula.is_some() || f.through.is_some() || !quadra_store_is_inverse(model, e, f) {
        return None;
    }
    let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty else {
        return None;
    };
    let target = entity_named(inner).and_then(|n| model.entity(&n))?;
    if target.kind != EntityKind::Entity {
        return None;
    }
    let back = f.via.clone().or_else(|| {
        target
            .fields
            .iter()
            .find(|x| {
                x.ty.as_ref()
                    .and_then(entity_named)
                    .map_or(false, |n| n == e.name || e.traits.contains(&n))
            })
            .map(|x| x.name.clone())
    })?;
    Some((target.name.clone(), back))
}

/// The cell of a collection of lines, bounded: its first `LINES_PAGE` records the user may
/// read (`v`, `names`), how many there are (`count`, `estimated` beyond the exact count of a
/// query), and `next`: the cursor of `POST /v1/query {"entity": …, "refs": {back: id},
/// "after": next}` for the others (none when they are all here). In memory the collection is
/// read whole, then cut the same way.
fn lines_cell(i: &mut Interp, model: &Model, rec: &V, field: &str, child: &str, back: &str) -> J {
    let V::Ref { id, .. } = rec else {
        return json!({ "error": "not a record" });
    };
    if !i.can("read", child, None, None) {
        return json!({ "hidden": true });
    }
    let (items, count, estimated, next): (Vec<V>, u64, bool, Option<String>) = if i.store.lazy() {
        let q = json!({ "entity": child, "fields": [], "refs": { back: id }, "limit": LINES_PAGE, "count": true });
        match query_rows(i, model, child, &q, &[], &[]) {
            Ok(page) => {
                let ent: Rc<str> = Rc::from(child);
                let items: Vec<V> = page["rows"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r["id"].as_u64())
                            .map(|id| V::Ref {
                                ent: ent.clone(),
                                id,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let count = page["total"].as_u64().unwrap_or(items.len() as u64);
                (
                    items,
                    count,
                    page["estimated"].as_bool().unwrap_or(false),
                    page["next"].as_str().map(str::to_string),
                )
            }
            Err((_, m)) => return json!({ "error": m }),
        }
    } else {
        let all = match i.get(rec, field, None, None) {
            Ok(V::List(items) | V::Set(items)) => items,
            Ok(_) => Rc::new(Vec::new()),
            Err(err) => return json!({ "error": describe(err) }),
        };
        let mut readable: Vec<V> = Vec::new();
        for it in all.iter() {
            if i.can("read", child, None, Some(it)) {
                readable.push(it.clone());
            }
        }
        let count = readable.len() as u64;
        let more = readable.len() > LINES_PAGE;
        readable.truncate(LINES_PAGE);
        // (the query's cursor in memory: how many rows came before, in id order)
        let next = more.then(|| encode_cursor(&json!({ "k": "o", "o": LINES_PAGE })));
        (readable, count, false, next)
    };
    let list = V::List(Rc::new(items.clone()));
    let names: Vec<String> = items.iter().map(|x| i.display(x)).collect();
    let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let d = if count > 3 {
        format!("{shown} … ({}{count})", if estimated { "~" } else { "" })
    } else {
        shown
    };
    let mut c = json!({ "v": json::wire(&list), "d": d, "count": count, "next": next });
    if !names.is_empty() {
        c["names"] = json!(names);
    }
    if estimated {
        c["estimated"] = json!(true);
    }
    c
}

/// The operations of a transaction (`create`, `update`, `action`), inside the caller's
/// transaction. Returns the ids of the records created with a `ref`.
pub fn apply_ops(
    i: &mut Interp,
    model: &Model,
    ops: &[J],
) -> Result<serde_json::Map<String, J>, Flow> {
    let created: RefCell<serde_json::Map<String, J>> = RefCell::new(serde_json::Map::new());
    let refs: RefCell<HashMap<String, V>> = RefCell::new(HashMap::new());
    for (n, op) in ops.iter().enumerate() {
        let fail = |m: String| Flow::Fail(format!("operation {}: {m}", n + 1));
        let ent = op["entity"]
            .as_str()
            .ok_or_else(|| fail("`entity` is required".into()))?;
        let e = model
            .entity(ent)
            .filter(|e| e.kind == EntityKind::Entity && !e.temporary && !e.is_ledger)
            .ok_or_else(|| fail(format!("unknown entity `{ent}`")))?;
        let lookup = |r: &str| refs.borrow().get(r).cloned();
        let empty = serde_json::Map::new();
        let values = op["values"].as_object().unwrap_or(&empty);
        match op["op"].as_str() {
            Some("create") => {
                let args = decode_fields(i, model, ent, values, &lookup).map_err(fail)?;
                let names: Vec<String> = args.iter().map(|(k, _)| k.clone()).collect();
                let rec = i.create(ent, args)?;
                i.check_permission("create", ent, None, Some(&rec))?;
                for k in &names {
                    i.coerce_field(&rec, k)?;
                }
                if let (Some(r), V::Ref { id, .. }) = (op["ref"].as_str(), &rec) {
                    created.borrow_mut().insert(r.to_string(), json!(id));
                    refs.borrow_mut().insert(r.to_string(), rec.clone());
                }
            }
            Some("update") => {
                let id = op["id"]
                    .as_u64()
                    .ok_or_else(|| fail("`id` is required".into()))?;
                let rec = V::Ref {
                    ent: Rc::from(ent),
                    id,
                };
                // the record must be of the entity the operation names (its rights were checked
                // for that one): an id of another entity does not exist here
                let of_it = i.store.exists(id, None)
                    && i.store.get(id).is_some_and(|r| {
                        &*r.ent == ent
                            || model
                                .entity(&r.ent)
                                .is_some_and(|a| model.implements(a, ent))
                    });
                if !of_it {
                    return Err(fail(format!("{ent}#{id} does not exist")));
                }
                for (k, v) in values {
                    let f = model
                        .find_field(e, k)
                        .ok_or_else(|| fail(format!("`{ent}` has no field `{k}`")))?;
                    let val = decode(i, model, f.ty.as_ref(), v, &lookup)
                        .map_err(|m| fail(format!("{ent}.{k}: {m}")))?;
                    let company = i.company;
                    i.set(&rec, k, val, company)?;
                    i.coerce_field(&rec, k)?;
                }
            }
            Some("action") => {
                let id = op["id"]
                    .as_u64()
                    .ok_or_else(|| fail("`id` is required".into()))?;
                let name = op["action"]
                    .as_str()
                    .ok_or_else(|| fail("`action` is required".into()))?;
                let rec = V::Ref {
                    ent: Rc::from(ent),
                    id,
                };
                // the record must be of the entity the operation names (its rights were checked
                // for that one): an id of another entity does not exist here
                let of_it = i.store.exists(id, None)
                    && i.store.get(id).is_some_and(|r| {
                        &*r.ent == ent
                            || model
                                .entity(&r.ent)
                                .is_some_and(|a| model.implements(a, ent))
                    });
                if !of_it {
                    return Err(fail(format!("{ent}#{id} does not exist")));
                }
                let action = e.actions.iter().find(|a| a.name == name);
                let is_flow = e
                    .flows
                    .iter()
                    .any(|fl| fl.transitions.iter().any(|t| t.by.as_deref() == Some(name)));
                if action.is_none() && !is_flow {
                    return Err(fail(format!("`{ent}` has no action `{name}`")));
                }
                let mut named = Vec::new();
                if let (Some(a), Some(args)) = (action, op["args"].as_object()) {
                    for (k, v) in args {
                        let p = a
                            .params
                            .iter()
                            .find(|p| &p.name == k)
                            .ok_or_else(|| fail(format!("`{name}` has no parameter `{k}`")))?;
                        named.push((
                            k.clone(),
                            decode(i, model, p.ty.as_ref(), v, &lookup)
                                .map_err(|m| fail(format!("{k}: {m}")))?,
                        ));
                    }
                }
                i.run_action(&rec, ent, name, action, Vec::new(), named)?;
            }
            Some("erase") => {
                let id = op["id"]
                    .as_u64()
                    .ok_or_else(|| fail("`id` is required".into()))?;
                let reason = op["reason"].as_str().unwrap_or("");
                let of_it =
                    i.store.exists(id, None) && i.store.get(id).is_some_and(|r| &*r.ent == ent);
                if !of_it {
                    return Err(fail(format!("{ent}#{id} does not exist")));
                }
                let rec = V::Ref {
                    ent: Rc::from(ent),
                    id,
                };
                i.erase(&rec, reason)?;
            }
            Some("delete") => {
                let id = op["id"]
                    .as_u64()
                    .ok_or_else(|| fail("`id` is required".into()))?;
                let of_it =
                    i.store.exists(id, None) && i.store.get(id).is_some_and(|r| &*r.ent == ent);
                if !of_it {
                    return Err(fail(format!("{ent}#{id} does not exist")));
                }
                i.delete(&V::Ref {
                    ent: Rc::from(ent),
                    id,
                })?;
            }
            Some("restore") => {
                let id = op["id"]
                    .as_u64()
                    .ok_or_else(|| fail("`id` is required".into()))?;
                i.restore(&V::Ref {
                    ent: Rc::from(ent),
                    id,
                })?;
            }
            _ => {
                return Err(fail(
                    "`op` must be create, update, action, erase, delete or restore".into(),
                ))
            }
        }
    }
    Ok(created.into_inner())
}

/// What a record holds about the person it is (GDPR art. 15 and 20): the record and its lines
/// as the API shows them, the past values of its `personal` fields with when and why they
/// were written, and the records that refer to it. Readable by whoever may read the record and
/// its history.
pub fn subject_json(i: &mut Interp, model: &Model, entity: &str, id: u64) -> Result<J, Fail> {
    let record = record_json(i, model, entity, id)?;
    let actual = i
        .store
        .get(id)
        .map(|r| r.ent.to_string())
        .unwrap_or_default();
    if !i.can_read_history(&actual) {
        return Err((
            403,
            format!("permission denied: cannot read the history of {actual}#{id}"),
        ));
    }
    let mut lines = Vec::new();
    for (le, lid) in i.lines_of(&actual, id) {
        if let Ok(l) = record_json(i, model, &le, lid) {
            lines.push(l);
        }
    }
    let e = model
        .entity(&actual)
        .ok_or((404, "not found".to_string()))?;
    let personal: Vec<String> = e
        .fields
        .iter()
        .filter(|f| f.personal)
        .map(|f| f.name.clone())
        .collect();
    i.store.history_from(id, 0);
    let entries: Vec<(u64, String, V)> = i
        .store
        .get(id)
        .map(|r| {
            r.history
                .iter()
                .filter(|(_, f, _)| personal.contains(f))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut history = Vec::new();
    for (tx, f, v) in entries {
        let (at, by) = i.store.commit_info(tx).unwrap_or_default();
        let by = by.map(|u| {
            i.display(&V::Ref {
                ent: Rc::from("User"),
                id: u,
            })
        });
        history.push(json!({
            "tx": tx,
            "at": at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "by": by,
            "field": f,
            "value": json::wire(&v),
        }));
    }
    let referred_by: Vec<J> = i
        .referring(&actual, id)
        .into_iter()
        .map(|(e, r, f)| json!({ "entity": e, "id": r, "field": f }))
        .collect();
    Ok(json!({
        "subject": { "entity": actual, "id": id },
        "record": record,
        "lines": lines,
        "personal_history": history,
        "referred_by": referred_by,
    }))
}

/// Whether a collection field is computed from the other side (`schema::is_inverse`).
fn quadra_store_is_inverse(model: &Model, owner: &quadra_ir::Entity, f: &quadra_ir::Field) -> bool {
    let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty else {
        return false;
    };
    let Ty::Path { segments, .. } = inner.as_ref() else {
        return false;
    };
    let Some(child) = model.entity(&segments[0]) else {
        return false;
    };
    if child.kind == EntityKind::Record {
        return false;
    }
    if f.through.is_some() || f.via.is_some() {
        return true;
    }
    child.fields.iter().any(|x| {
        x.ty.as_ref()
            .and_then(entity_named)
            .map_or(false, |n| n == owner.name || owner.traits.contains(&n))
    })
}

/// The entity a type names, through `?`: `Customer`, `Customer?`.
fn entity_named(t: &Ty) -> Option<String> {
    match t {
        Ty::Path { segments, .. } => segments.last().cloned(),
        Ty::Optional(i) => entity_named(i),
        _ => None,
    }
}
