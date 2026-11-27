//! Grouping and pivots of `POST /v1/query` (`group_by`), and what the queries of the grid share
//! with them: the rows of a group (`group_filter`) and the full-text search (`search`).
//!
//! The groups are computed by the database when the query translates (`Interp::sql_groups`:
//! one `GROUP BY` with the row permissions, the `where`, the search and the group's filter), the
//! interpreter's way otherwise, on at most `COUNT_CAP` rows (an error beyond: never a part of
//! the rows). Both give the same groups in the same order: a key is a value at the end of a
//! field path (`query::group_value`), groups are ordered as the grid orders cells (text by its
//! ASCII lower case, a record by what it is shown by, then its id; none last), or by their row
//! count or an aggregate.

use super::*;
use quadra_runtime::query::{AggOut, Bucket, GroupKey, GroupOrder, GroupOut, TextSearch};
use quadra_runtime::KeyVal;
use std::collections::BTreeMap;

/// Columns of a pivot at most (the others are reported, not shown).
pub const PIVOT_COLUMNS: usize = 50;

/// The kind of a key, from the type at the end of its path.
#[derive(Clone, Copy, PartialEq, Debug)]
enum KeyKind {
    Ref,
    Text,
    Int,
    Bool,
    Date,
}

/// A key of `group_by` or `group_filter`, checked against the model and what the user may
/// see: (the key, the entity a reference points to, its kind).
fn parse_key(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    item: &J,
) -> Result<(GroupKey, Option<String>, KeyKind), Fail> {
    let (path, bucket) = match item {
        J::String(s) => (s.clone(), None),
        J::Object(o) => (
            o.get("field")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            o.get("bucket").and_then(|x| x.as_str()).map(str::to_string),
        ),
        _ => {
            return Err((
                400,
                "`group_by`: a field path, or {\"field\": …, \"bucket\": …}".into(),
            ))
        }
    };
    let segs: Vec<String> = path.split('.').map(str::to_string).collect();
    if path.is_empty() || segs.len() > 5 {
        return Err((
            400,
            format!("`{path}`: a field path of at most 5 fields (4 relations)"),
        ));
    }
    let mut ent = entity.to_string();
    let mut last: Option<quadra_ir::Field> = None;
    let mut hidden_path = false;
    for (k, seg) in segs.iter().enumerate() {
        let e = model
            .entity(&ent)
            .ok_or((400, format!("`{path}`: `{ent}` is not an entity")))?;
        let f = model
            .find_field(e, seg)
            .ok_or((400, format!("`{path}`: `{ent}` has no field `{seg}`")))?
            .clone();
        if i.hidden(&ent, seg) {
            return Err((403, format!("`{path}`: `{ent}.{seg}` is hidden")));
        }
        if k + 1 < segs.len() {
            let next = f.ty.as_ref().and_then(entity_named).filter(|n| {
                model
                    .entity(n)
                    .is_some_and(|x| x.kind == EntityKind::Entity)
            });
            let Some(next) = next else {
                return Err((400, format!("`{path}`: `{seg}` is not a reference")));
            };
            if !i.read_unconditional(&next) {
                hidden_path = true;
            }
            ent = next;
        }
        last = Some(f);
    }
    let f = last.unwrap();
    let base = match &f.ty {
        Some(Ty::Optional(t)) => Some(t.as_ref().clone()),
        t => t.clone(),
    };
    let named = |n: &str| matches!(&base, Some(Ty::Path { segments, .. }) if segments.last().map(String::as_str) == Some(n));
    let target = base.as_ref().and_then(entity_named).filter(|n| {
        model
            .entity(n)
            .is_some_and(|x| x.kind == EntityKind::Entity)
    });
    let kind = if target.is_some() {
        KeyKind::Ref
    } else if matches!(base, Some(Ty::Enum(_))) || quadra_runtime::query::text_type(base.as_ref()) {
        KeyKind::Text
    } else if named("Int") {
        KeyKind::Int
    } else if named("Bool") {
        KeyKind::Bool
    } else if named("Date") || named("DateTime") {
        KeyKind::Date
    } else {
        return Err((400, format!("`{path}`: grouping by a record, text, a choice, an integer, a yes/no or a date (not this field)")));
    };
    let bucket = match bucket.as_deref() {
        None | Some("") => None,
        Some(b) if kind == KeyKind::Date => Some(Bucket::parse(b).ok_or((
            400,
            format!("`{path}`: bucket `{b}`: day, week, month, quarter or year"),
        ))?),
        Some(_) => return Err((400, format!("`{path}`: only a date has a bucket"))),
    };
    Ok((
        GroupKey {
            path: segs,
            bucket,
            hidden_path,
        },
        target,
        kind,
    ))
}

/// `group_filter`: the rows of a group (each key with its value; null: no value).
pub(crate) fn group_filter(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    q: &J,
) -> Result<Vec<(GroupKey, Option<String>)>, Fail> {
    let mut out = Vec::new();
    let Some(list) = q["group_filter"].as_array() else {
        return Ok(out);
    };
    for item in list.iter().take(4) {
        let (k, _, _) = parse_key(i, model, entity, item)?;
        let v = match &item["value"] {
            J::Null => None,
            J::String(s) => Some(s.clone()),
            J::Number(n) => Some(n.to_string()),
            J::Bool(b) => Some(b.to_string()),
            _ => {
                return Err((
                    400,
                    "`group_filter`: a value is text, a number, true/false or null".into(),
                ))
            }
        };
        out.push((k, v));
    }
    Ok(out)
}

/// `search`: every word begins a word of a stored text field the user may see.
pub(crate) fn search_of(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    q: &J,
) -> Result<Option<TextSearch>, Fail> {
    let Some(text) = q["search"].as_str() else {
        return Ok(None);
    };
    if text.chars().count() > 200 {
        return Err((400, "`search`: at most 200 characters".into()));
    }
    let words = quadra_runtime::query::search_words(text);
    if words.is_empty() {
        return Ok(None);
    }
    let fields: Vec<String> = quadra_runtime::query::searchable(model, entity)
        .into_iter()
        .filter(|f| !i.hidden(entity, f))
        .collect();
    Ok(Some(TextSearch { words, fields }))
}

/// Whether a record is in a group and matches the search (the interpreter's way).
pub(crate) fn in_scope(
    i: &mut Interp,
    entity: &str,
    rec: &V,
    keys: &[(GroupKey, Option<String>)],
    search: Option<&TextSearch>,
) -> Result<bool, Fail> {
    for (k, want) in keys {
        match i.group_key_of(rec, entity, k) {
            Ok(v) if v == KeyVal::Val(want.clone()) => {}
            Ok(_) => return Ok(false),
            Err(e) => return Err((422, format!("group_filter: {}", describe(e)))),
        }
    }
    if let Some(s) = search {
        if !i.searched(rec, s) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How groups are ordered by a key value (as SQL orders them: none last).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Sort {
    Num(i64),
    Bool(bool),
    Text(String, String),
    Ref(String, u64),
    Plain(String),
}

/// A group computed by the interpreter: its key values, rows.
struct Found {
    keys: Vec<KeyVal>,
    ids: Vec<u64>,
}

/// One key of the request: its path, the referenced entity, its kind.
struct KeyInfo {
    key: GroupKey,
    target: Option<String>,
    kind: KeyKind,
}

/// The ordering value of a key (None: no value or hidden, last).
fn sort_of(i: &mut Interp, model: &Model, k: &KeyInfo, v: &KeyVal) -> Option<Sort> {
    let KeyVal::Val(Some(s)) = v else { return None };
    Some(match k.kind {
        KeyKind::Int => Sort::Num(s.parse().unwrap_or(0)),
        KeyKind::Bool => Sort::Bool(s == "true"),
        KeyKind::Text => Sort::Text(s.to_ascii_lowercase(), s.clone()),
        KeyKind::Date => Sort::Plain(s.clone()),
        KeyKind::Ref => {
            let id: u64 = s.parse().unwrap_or(0);
            let ent = k.target.clone().unwrap_or_default();
            // what SQL orders a reference by: its number, code, name or login (the first it has)
            let mut shown = None;
            let rec = V::Ref {
                ent: Rc::from(ent.as_str()),
                id,
            };
            if let Some(e) = model.entity(&ent) {
                for f in ["number", "code", "name", "login"] {
                    if model.find_field(e, f).is_some_and(|x| x.formula.is_none()) {
                        if let Ok(v) = i.get(&rec, f, None, None) {
                            if !v.is_none() {
                                shown = Some(v.to_string().to_ascii_lowercase());
                                break;
                            }
                        }
                    }
                }
            }
            Sort::Ref(
                shown.unwrap_or_else(|| format!("{}#{id}", ent.to_ascii_lowercase())),
                id,
            )
        }
    })
}

/// The JSON of a key value: `{value, d}` (`hidden` when the path goes through what the user may
/// not see).
fn key_json(i: &mut Interp, k: &KeyInfo, v: &KeyVal) -> J {
    match v {
        KeyVal::Hidden => json!({ "value": null, "d": "", "hidden": true }),
        KeyVal::Val(None) => json!({ "value": null, "d": "" }),
        KeyVal::Val(Some(s)) => {
            let d = match k.kind {
                KeyKind::Ref => {
                    let rec = V::Ref {
                        ent: Rc::from(k.target.clone().unwrap_or_default().as_str()),
                        id: s.parse().unwrap_or(0),
                    };
                    i.display(&rec)
                }
                KeyKind::Date if k.key.bucket.unwrap_or(Bucket::Day) == Bucket::Day => {
                    match chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                        Ok(d) => i.display(&V::Date(d)),
                        Err(_) => s.clone(),
                    }
                }
                KeyKind::Date if k.key.bucket == Some(Bucket::Week) => format!("week of {s}"),
                KeyKind::Bool => i.display(&V::Bool(s == "true")),
                _ => s.clone(),
            };
            json!({ "value": s, "d": d })
        }
    }
}

/// The `group_filter` that selects a group's rows.
fn filter_json(keys: &[&KeyInfo], vals: &[KeyVal]) -> J {
    J::Array(
        keys.iter()
            .zip(vals.iter())
            .map(|(k, v)| {
                let value = match v {
                    KeyVal::Val(Some(s)) => json!(s),
                    _ => J::Null,
                };
                json!({ "field": k.key.path.join("."), "bucket": k.key.bucket.map(|b| b.name()), "value": value })
            })
            .collect(),
    )
}

/// What an aggregate is about: (field path, function), hidden from the user or not.
struct AggInfo {
    path: String,
    func: String,
    hidden: bool,
}

/// The aggregates of a group computed by SQL.
fn aggs_json(i: &mut Interp, model: &Model, entity: &str, aggs: &[AggInfo], outs: &[AggOut]) -> J {
    let mut m = serde_json::Map::new();
    let mut k = 0;
    for a in aggs {
        if a.hidden {
            m.insert(a.path.clone(), json!({ "fn": a.func, "hidden": true }));
            continue;
        }
        let out = &outs[k];
        k += 1;
        let c = if matches!(out.kind, "money" | "qty") && out.units > 1 {
            json!({ "fn": a.func, "error": several(out.kind) })
        } else {
            match aggregate_value(i, model, entity, &a.path, &a.func, out) {
                Some(v) => {
                    let mut c = cell(i, &v.normalized());
                    c["fn"] = json!(a.func);
                    c
                }
                None => json!({ "fn": a.func, "error": "not computed" }),
            }
        };
        m.insert(a.path.clone(), c);
    }
    J::Object(m)
}

fn several(kind: &str) -> &'static str {
    if kind == "money" {
        "several currencies in this group"
    } else {
        "several units in this group"
    }
}

/// Whether the field at the end of a path is declared `Decimal` (an integer value in it is a
/// decimal, as SQL reads the column).
fn decimal_field(model: &Model, entity: &str, path: &str) -> bool {
    let mut ent = entity.to_string();
    let mut last = None;
    for seg in path.split('.') {
        let Some(f) = model.entity(&ent).and_then(|e| model.find_field(e, seg)) else {
            return false;
        };
        if let Some(n) = f.ty.as_ref().and_then(entity_named) {
            ent = n;
        }
        last = Some(f);
    }
    let t = match last.and_then(|f| f.ty.as_ref()) {
        Some(Ty::Optional(t)) => Some(t.as_ref()),
        t => t,
    };
    matches!(t, Some(Ty::Path { segments, .. }) if segments.last().map(String::as_str) == Some("Decimal"))
}

/// The aggregates of a group computed by the interpreter (the same values and errors as SQL's).
fn aggs_engine(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    aggs: &[AggInfo],
    ids: &[u64],
) -> (J, Vec<Option<V>>) {
    let mut m = serde_json::Map::new();
    let mut vals = Vec::new();
    for a in aggs {
        if a.hidden {
            m.insert(a.path.clone(), json!({ "fn": a.func, "hidden": true }));
            vals.push(None);
            continue;
        }
        // amounts of several currencies (quantities of several units): an error, as in SQL
        let mut units: std::collections::BTreeSet<String> = Default::default();
        let mut kind = "";
        for id in ids {
            let mut cur = V::Ref {
                ent: Rc::from(entity),
                id: *id,
            };
            for seg in a.path.split('.') {
                cur = match &cur {
                    V::None => V::None,
                    _ => i.get(&cur, seg, None, None).unwrap_or(V::None),
                };
            }
            match &cur {
                V::Money { cur, .. } => {
                    kind = "money";
                    units.insert(cur.to_string());
                }
                V::Qty { unit, .. } => {
                    kind = "qty";
                    units.insert(unit.to_string());
                }
                _ => {}
            }
        }
        if units.len() > 1 && a.func != "count" {
            m.insert(
                a.path.clone(),
                json!({ "fn": a.func, "error": several(kind) }),
            );
            vals.push(None);
            continue;
        }
        match aggregate(i, entity, ids, &a.path, &a.func) {
            Ok(v) => {
                let v = match v {
                    V::Int(n) if a.func != "count" && decimal_field(model, entity, &a.path) => {
                        V::Dec(n.into())
                    }
                    v => v,
                };
                let v = v.normalized();
                let mut c = cell(i, &v);
                c["fn"] = json!(a.func);
                m.insert(a.path.clone(), c);
                vals.push(Some(v));
            }
            Err(e) => {
                m.insert(a.path.clone(), json!({ "fn": a.func, "error": e }));
                vals.push(None);
            }
        }
    }
    (J::Object(m), vals)
}

/// The number groups are ordered by for an aggregate (None: last).
#[derive(Clone, PartialEq, PartialOrd, Debug)]
enum AggSort {
    Num(rust_decimal::Decimal),
    Text(String),
}

fn agg_sort(v: &Option<V>) -> Option<AggSort> {
    Some(match v.as_ref()? {
        V::Int(n) => AggSort::Num((*n).into()),
        V::Duration(n) => AggSort::Num((*n).into()),
        V::Dec(d) | V::Pct(d) => AggSort::Num(*d),
        V::Money { amt, .. } => AggSort::Num(*amt),
        V::Qty { v, .. } => AggSort::Num(*v),
        V::Date(d) => AggSort::Text(d.format("%Y-%m-%d").to_string()),
        V::DateTime(t) => AggSort::Text(t.format("%Y-%m-%d %H:%M:%S").to_string()),
        V::Text(t) => AggSort::Text(t.to_string()),
        _ => return None,
    })
}

/// `POST /v1/query` with `group_by` (see API.md): a page of groups of the rows, each with its
/// key values, row count, aggregates and the `group_filter` of its rows; with `pivot`, the
/// second key's values are columns (at most `PIVOT_COLUMNS`, `more_columns` beyond).
pub fn group_rows(
    i: &mut Interp,
    model: &Model,
    entity: &str,
    q: &J,
    items: &[(String, String)],
) -> Result<J, Fail> {
    let entity = entity.to_string();
    if !i.can("read", &entity, None, None) {
        return Err((403, format!("permission denied: cannot read {entity}")));
    }
    let list = q["group_by"]
        .as_array()
        .ok_or((400, "`group_by`: a list of 1 or 2 keys".to_string()))?;
    if list.is_empty() || list.len() > 2 {
        return Err((400, "`group_by`: 1 or 2 keys".into()));
    }
    let mut keys: Vec<KeyInfo> = Vec::new();
    for item in list {
        let (key, target, kind) = parse_key(i, model, &entity, item)?;
        keys.push(KeyInfo { key, target, kind });
    }
    let pivot = q["pivot"].as_bool().unwrap_or(false);
    if pivot && keys.len() != 2 {
        return Err((400, "`pivot`: two keys (rows, columns)".into()));
    }
    let mut aggs: Vec<AggInfo> = Vec::new();
    if let Some(o) = q["aggregates"].as_object() {
        for (path, f) in o.iter().take(20) {
            let func = f.as_str().unwrap_or("").to_string();
            if !matches!(func.as_str(), "sum" | "count" | "min" | "max" | "avg") {
                return Err((
                    400,
                    format!("aggregate of `{path}`: sum, count, min, max or avg"),
                ));
            }
            // a hidden field on the path: hidden in every group
            let mut ent = entity.clone();
            let mut hidden = false;
            for seg in path.split('.') {
                let Some(f) = model.entity(&ent).and_then(|e| model.find_field(e, seg)) else {
                    return Err((
                        400,
                        format!("aggregate of `{path}`: `{ent}` has no field `{seg}`"),
                    ));
                };
                if i.hidden(&ent, seg) {
                    hidden = true;
                }
                if let Some(n) = f.ty.as_ref().and_then(entity_named) {
                    ent = n;
                }
            }
            aggs.push(AggInfo {
                path: path.clone(),
                func,
                hidden,
            });
        }
    }
    let (order, desc) = {
        let o = &q["group_order"];
        let desc = o["desc"].as_bool().unwrap_or(false);
        match o["by"].as_str().unwrap_or("key") {
            "key" => (GroupOrder::Key, desc),
            "count" => (GroupOrder::Count, desc),
            path => {
                let n = aggs
                    .iter()
                    .filter(|a| !a.hidden)
                    .position(|a| a.path == path)
                    .ok_or((
                        400,
                        format!("`group_order`: `{path}` is not one of the aggregates"),
                    ))?;
                if aggs
                    .iter()
                    .filter(|a| !a.hidden)
                    .nth(n)
                    .is_some_and(|a| a.func == "avg")
                {
                    return Err((400, "`group_order`: not by an average".into()));
                }
                (GroupOrder::Agg(n), desc)
            }
        }
    };
    let limit = q["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
    let offset = match q["after"].as_str() {
        Some(c) if !c.is_empty() => decode_cursor(c)?["g"]
            .as_u64()
            .ok_or((400, "`after`: not a cursor of this query".to_string()))?
            as usize,
        _ => q["offset"].as_u64().unwrap_or(0) as usize,
    };
    // the rows grouped: those of the query
    let refs = refs_of(model, &entity, q)?;
    let wexpr = where_expr(model, &entity, items);
    let ids: Option<Vec<u64>> = q["ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).take(1000).collect());
    let filter = group_filter(i, model, &entity, q)?;
    let search = search_of(i, model, &entity, q)?;
    let flt = filters_of(i, model, &entity, q)?;
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
    let sql_aggs: Vec<quadra_runtime::query::Agg> = aggs
        .iter()
        .filter(|a| !a.hidden)
        .map(|a| quadra_runtime::query::Agg {
            of: quadra_runtime::query::AggOf::Path(a.path.split('.').map(str::to_string).collect()),
            func: match a.func.as_str() {
                "sum" | "avg" => quadra_runtime::query::AggFn::Sum,
                "min" => quadra_runtime::query::AggFn::Min,
                "max" => quadra_runtime::query::AggFn::Max,
                _ => quadra_runtime::query::AggFn::Count,
            },
        })
        .collect();
    // a path through records the user may not always read: the interpreter (it hides them)
    let through_rows = keys.iter().any(|k| k.key.hidden_path);

    let gk = |ks: &[&KeyInfo]| -> Vec<GroupKey> { ks.iter().map(|k| k.key.clone()).collect() };
    let mut out = json!({ "entity": entity, "base": i.store.base, "group_by": keys.iter().map(|k| json!({ "field": k.key.path.join("."), "bucket": k.key.bucket.map(|b| b.name()), "kind": match k.kind { KeyKind::Ref => "ref", KeyKind::Text => "text", KeyKind::Int => "int", KeyKind::Bool => "bool", KeyKind::Date => "date" }, "entity": k.target })).collect::<Vec<_>>(), "pivot": pivot });
    let row_keys: Vec<&KeyInfo> = if pivot {
        vec![&keys[0]]
    } else {
        keys.iter().collect()
    };

    // ---- SQL ----
    if !through_rows {
        let mut ctx = flt.ctx();
        let sql = (|| -> Result<Option<J>, Fail> {
            let err = |e: Flow| (422, format!("where: {}", describe(e)));
            let Some(page) = i
                .sql_groups(
                    &spec,
                    &mut ctx,
                    &gk(&row_keys),
                    &sql_aggs,
                    order,
                    desc,
                    offset,
                    limit,
                    Vec::new(),
                )
                .map_err(err)?
            else {
                return Ok(None);
            };
            let mut groups: Vec<J> = Vec::new();
            prefetch_keys(i, &row_keys, page.groups.iter().map(|g| &g.keys));
            for g in &page.groups {
                let vals: Vec<KeyVal> = g
                    .keys
                    .iter()
                    .zip(row_keys.iter())
                    .map(|(v, k)| KeyVal::Val(v.as_ref().map(|s| canonical(k.kind, s))))
                    .collect();
                let kj: Vec<J> = row_keys
                    .iter()
                    .zip(vals.iter())
                    .map(|(k, v)| key_json(i, k, v))
                    .collect();
                groups.push(json!({ "keys": kj, "count": g.count, "aggregates": aggs_json(i, model, &entity, &aggs, &g.aggs), "filter": filter_json(&row_keys, &vals) }));
            }
            let mut o = json!({ "groups": groups, "groups_total": page.total });
            if pivot {
                let col = [&keys[1]];
                let Some(cols) = i
                    .sql_groups(
                        &spec,
                        &mut ctx,
                        &gk(&col),
                        &sql_aggs,
                        GroupOrder::Key,
                        false,
                        0,
                        PIVOT_COLUMNS + 1,
                        Vec::new(),
                    )
                    .map_err(err)?
                else {
                    return Ok(None);
                };
                let more = cols.groups.len() > PIVOT_COLUMNS;
                let shown: Vec<&GroupOut> = cols.groups.iter().take(PIVOT_COLUMNS).collect();
                prefetch_keys(i, &col, shown.iter().map(|g| &g.keys));
                let col_vals: Vec<Option<String>> = shown
                    .iter()
                    .map(|g| g.keys[0].as_ref().map(|s| canonical(keys[1].kind, s)))
                    .collect();
                let mut columns = Vec::new();
                for (g, v) in shown.iter().zip(col_vals.iter()) {
                    let kv = KeyVal::Val(v.clone());
                    columns.push(json!({ "key": key_json(i, &keys[1], &kv), "count": g.count, "aggregates": aggs_json(i, model, &entity, &aggs, &g.aggs), "filter": filter_json(&col, &[kv]) }));
                }
                let row_vals: Vec<Option<String>> = page
                    .groups
                    .iter()
                    .map(|g| g.keys[0].as_ref().map(|s| canonical(keys[0].kind, s)))
                    .collect();
                let mut cells: HashMap<(Option<String>, Option<String>), GroupOut> = HashMap::new();
                if !row_vals.is_empty() && !col_vals.is_empty() {
                    let both = [&keys[0], &keys[1]];
                    let n = row_vals.len() * col_vals.len();
                    let Some(c) = i
                        .sql_groups(
                            &spec,
                            &mut ctx,
                            &gk(&both),
                            &sql_aggs,
                            GroupOrder::Key,
                            false,
                            0,
                            n,
                            vec![Some(row_vals.clone()), Some(col_vals.clone())],
                        )
                        .map_err(err)?
                    else {
                        return Ok(None);
                    };
                    for g in c.groups {
                        let a = g.keys[0].as_ref().map(|s| canonical(keys[0].kind, s));
                        let b = g.keys[1].as_ref().map(|s| canonical(keys[1].kind, s));
                        cells.insert((a, b), g);
                    }
                }
                for (gi, rv) in row_vals.iter().enumerate() {
                    let mut cs = Vec::new();
                    for cv in &col_vals {
                        cs.push(match cells.get(&(rv.clone(), cv.clone())) {
                            Some(g) => json!({ "count": g.count, "aggregates": aggs_json(i, model, &entity, &aggs, &g.aggs) }),
                            None => J::Null,
                        });
                    }
                    o["groups"][gi]["cells"] = J::Array(cs);
                    o["groups"][gi]["filter"] =
                        filter_json(&[&keys[0]], &[KeyVal::Val(rv.clone())]);
                }
                o["columns"] = J::Array(columns);
                o["more_columns"] = json!(more);
                o["columns_total"] = json!(cols.total);
            }
            Ok(Some(o))
        })()?;
        if let Some(o) = sql {
            for (k, v) in o.as_object().unwrap() {
                out[k] = v.clone();
            }
            out["next"] = next_cursor(offset, limit, out["groups_total"].as_u64().unwrap_or(0));
            out["sql"] = json!(true);
            return Ok(out);
        }
    }

    // ---- the interpreter's way, on at most COUNT_CAP rows ----
    let rows: Vec<u64> = {
        let mut ctx = flt.ctx();
        let mut s2 = spec.clone();
        s2.order = Vec::new();
        match i
            .sql_page(&s2, &mut ctx, None, 0, COUNT_CAP as usize + 1, usize::MAX)
            .map_err(|e| (422, format!("where: {}", describe(e))))?
        {
            Some(p) => p.ids,
            None => {
                let all = match &ids {
                    Some(a) => a.clone(),
                    None => i.ids(&entity, None),
                };
                if i.store.lazy() && all.len() > SCAN_CAP {
                    return Err((
                        422,
                        format!("more than {SCAN_CAP} rows to group this way: filter them further"),
                    ));
                }
                let mut kept = Vec::new();
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
                    if !in_scope(i, &entity, &rec, &filter, search.as_ref())? {
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
                    kept.push(id);
                }
                kept
            }
        }
    };
    if rows.len() > COUNT_CAP as usize {
        return Err((422, format!("more than {COUNT_CAP} rows to group this way (a condition or a permission that does not translate to SQL, or a path through records the user may not all read): filter them further")));
    }
    i.store.prefetch_as(&entity, &rows);
    let mut key_of: Vec<Vec<KeyVal>> = Vec::new();
    for id in &rows {
        let rec = V::Ref {
            ent: Rc::from(entity.as_str()),
            id: *id,
        };
        let mut ks = Vec::new();
        for k in &keys {
            ks.push(
                i.group_key_of(&rec, &entity, &k.key)
                    .map_err(|e| (422, format!("group_by: {}", describe(e))))?,
            );
        }
        key_of.push(ks);
    }
    // groups of `which` keys (indexes into `keys`), in their order
    let group_of = |i: &mut Interp,
                    which: &[usize],
                    order: GroupOrder,
                    desc: bool|
     -> Vec<(Found, J, Vec<Option<V>>)> {
        let mut map: BTreeMap<Vec<KeyVal>, Vec<u64>> = BTreeMap::new();
        for (n, id) in rows.iter().enumerate() {
            map.entry(which.iter().map(|w| key_of[n][*w].clone()).collect())
                .or_default()
                .push(*id);
        }
        let mut found: Vec<(Found, J, Vec<Option<V>>, Vec<Option<Sort>>)> = Vec::new();
        for (ks, ids) in map {
            let (aj, vals) = aggs_engine(i, model, &entity, &aggs, &ids);
            let sorts: Vec<Option<Sort>> = which
                .iter()
                .zip(ks.iter())
                .map(|(w, v)| sort_of(i, model, &keys[*w], v))
                .collect();
            found.push((Found { keys: ks, ids }, aj, vals, sorts));
        }
        let visible: Vec<usize> = aggs
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.hidden)
            .map(|(n, _)| n)
            .collect();
        let key_cmp = |a: &[Option<Sort>],
                       ak: &[KeyVal],
                       b: &[Option<Sort>],
                       bk: &[KeyVal],
                       kdesc: bool|
         -> std::cmp::Ordering {
            for n in 0..a.len() {
                // none (and hidden) last, first when descending
                let o = match (&a[n], &b[n]) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => ak[n].cmp(&bk[n]),
                };
                let o = if kdesc { o.reverse() } else { o };
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        };
        found.sort_by(|a, b| {
            let first = match order {
                GroupOrder::Key | GroupOrder::Ranked { .. } => std::cmp::Ordering::Equal,
                GroupOrder::Count => {
                    let o = a.0.ids.len().cmp(&b.0.ids.len());
                    if desc {
                        o.reverse()
                    } else {
                        o
                    }
                }
                GroupOrder::Agg(n) => {
                    let at = visible.get(n).copied().unwrap_or(0);
                    match (agg_sort(&a.2[at]), agg_sort(&b.2[at])) {
                        (Some(x), Some(y)) => {
                            let o = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
                            if desc {
                                o.reverse()
                            } else {
                                o
                            }
                        }
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    }
                }
            };
            first.then_with(|| {
                key_cmp(
                    &a.3,
                    &a.0.keys,
                    &b.3,
                    &b.0.keys,
                    desc && order == GroupOrder::Key,
                )
            })
        });
        found.into_iter().map(|(f, j, v, _)| (f, j, v)).collect()
    };
    let which: Vec<usize> = if pivot {
        vec![0]
    } else {
        (0..keys.len()).collect()
    };
    let all = group_of(i, &which, order, desc);
    let total = all.len();
    let mut groups: Vec<J> = Vec::new();
    let page: Vec<&(Found, J, Vec<Option<V>>)> = all.iter().skip(offset).take(limit).collect();
    for (f, aj, _) in &page {
        let infos: Vec<&KeyInfo> = which.iter().map(|w| &keys[*w]).collect();
        let kj: Vec<J> = infos
            .iter()
            .zip(f.keys.iter())
            .map(|(k, v)| key_json(i, k, v))
            .collect();
        groups.push(json!({ "keys": kj, "count": f.ids.len(), "aggregates": aj, "filter": filter_json(&infos, &f.keys) }));
    }
    if pivot {
        let cols = group_of(i, &[1], GroupOrder::Key, false);
        let more = cols.len() > PIVOT_COLUMNS;
        let shown: Vec<&(Found, J, Vec<Option<V>>)> = cols.iter().take(PIVOT_COLUMNS).collect();
        let mut columns = Vec::new();
        for (f, aj, _) in &shown {
            columns.push(json!({ "key": key_json(i, &keys[1], &f.keys[0]), "count": f.ids.len(), "aggregates": aj, "filter": filter_json(&[&keys[1]], &f.keys) }));
        }
        let cells = group_of(i, &[0, 1], GroupOrder::Key, false);
        for (gi, (f, _, _)) in page.iter().enumerate() {
            let mut cs = Vec::new();
            for (c, _, _) in &shown {
                cs.push(
                    match cells
                        .iter()
                        .find(|(x, _, _)| x.keys[0] == f.keys[0] && x.keys[1] == c.keys[0])
                    {
                        Some((x, aj, _)) => json!({ "count": x.ids.len(), "aggregates": aj }),
                        None => J::Null,
                    },
                );
            }
            groups[gi]["cells"] = J::Array(cs);
        }
        out["columns"] = J::Array(columns);
        out["more_columns"] = json!(more);
        out["columns_total"] = json!(cols.len());
    }
    out["groups"] = J::Array(groups);
    out["groups_total"] = json!(total);
    out["next"] = next_cursor(offset, limit, total as u64);
    out["sql"] = json!(false);
    Ok(out)
}

/// The records a page of groups shows (its references), loaded at once.
fn prefetch_keys<'a>(
    i: &mut Interp,
    keys: &[&KeyInfo],
    groups: impl Iterator<Item = &'a Vec<Option<String>>>,
) {
    let mut want: Vec<Vec<u64>> = vec![Vec::new(); keys.len()];
    for g in groups {
        for (n, v) in g.iter().enumerate() {
            if let (Some(k), Some(v)) = (keys.get(n), v) {
                if k.kind == KeyKind::Ref {
                    want[n].extend(v.parse::<u64>().ok());
                }
            }
        }
    }
    for (k, ids) in keys.iter().zip(want) {
        if let (Some(t), false) = (&k.target, ids.is_empty()) {
            i.store.prefetch_as(t, &ids);
        }
    }
}

/// The key value SQL gave, as the interpreter writes it (`true`/`false`; SQLite gives booleans
/// as `1`/`0` when they are not cast).
fn canonical(kind: KeyKind, s: &str) -> String {
    match (kind, s) {
        (KeyKind::Bool, "1") => "true".into(),
        (KeyKind::Bool, "0") => "false".into(),
        _ => s.to_string(),
    }
}

fn next_cursor(offset: usize, limit: usize, total: u64) -> J {
    if ((offset + limit) as u64) < total {
        json!(encode_cursor(&json!({ "g": offset + limit })))
    } else {
        J::Null
    }
}

// ---- filters of apps ---------------------------------------------------------------------

/// `filters`: conditions on field paths, as the global filters of an app give them
/// (`[{"path": "customer.region", "op": "in", "value": [3, 5]}]`). Each is a condition of the
/// query like its `where`, with the values bound apart (`__r.customer.region in __fv0`): the
/// condition does not change with the values, and is translated to SQL like any other.
///
/// Operators: `eq` (a value; null: no value), `in` (a list of values), `range` (`[from, to]`,
/// either may be null: dates, date-times, numbers, amounts), `contains` (text), `true` (a
/// yes/no field that holds). A path goes through references; through a hidden field it is
/// refused.
pub struct Filters {
    pub binder: quadra_runtime::query::Binder,
    pub exprs: Vec<quadra_ir::Expr>,
    vals: Vec<(String, V)>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// A context with the filters' values, for the queries of the request.
    pub fn ctx(&self) -> quadra_runtime::Ctx {
        let mut c = quadra_runtime::Ctx::default();
        c.scopes.push(Default::default());
        for (n, v) in &self.vals {
            c.bind(n, v.clone());
        }
        c
    }

    pub fn conds(&self) -> Vec<quadra_runtime::query::Cond<'_>> {
        self.exprs
            .iter()
            .map(|e| quadra_runtime::query::Cond {
                binder: &self.binder,
                expr: e,
                negate: false,
            })
            .collect()
    }

    /// Whether a record passes every filter (the interpreter's way).
    pub fn holds(&self, i: &mut Interp, rec: &V) -> Result<bool, Fail> {
        if self.exprs.is_empty() {
            return Ok(true);
        }
        let mut c = self.ctx();
        c.bind("__r", rec.clone());
        for e in &self.exprs {
            match i.eval(e, &mut c) {
                Ok(v) if v.truthy() => {}
                Ok(_) => return Ok(false),
                Err(err) => return Err((422, format!("filters: {}", describe(err)))),
            }
        }
        Ok(true)
    }
}

pub fn filters_of(i: &mut Interp, model: &Model, entity: &str, q: &J) -> Result<Filters, Fail> {
    use quadra_ir::{Arg, BinOp, Expr, ExprKind};
    let mut out = Filters {
        binder: quadra_runtime::query::Binder::Param("__r".into()),
        exprs: Vec::new(),
        vals: Vec::new(),
    };
    let list = match &q["filters"] {
        J::Null => return Ok(out),
        J::Array(a) if a.len() <= 20 => a,
        _ => {
            return Err((
                400,
                "`filters`: at most 20 conditions {path, op, value}".into(),
            ))
        }
    };
    let Some(e) = model.entity(entity) else {
        return Err((404, format!("unknown entity `{entity}`")));
    };
    let span = e.span;
    let mk = |kind: ExprKind| Expr { kind, span };
    for (n, f) in list.iter().enumerate() {
        let path = f["path"].as_str().unwrap_or("");
        let op = f["op"].as_str().unwrap_or("eq");
        let at = |m: String| (400, format!("filters, `{path}`: {m}"));
        // the path, from the row
        let segs: Vec<&str> = path.split('.').collect();
        if path.is_empty() || segs.len() > 5 {
            return Err(at("a field path of at most 5 fields".into()));
        }
        let mut ent = entity.to_string();
        let mut x = mk(ExprKind::Name("__r".into()));
        let mut last: Option<Ty> = None;
        let mut prev_opt = false;
        for (k, seg) in segs.iter().enumerate() {
            let en = model
                .entity(&ent)
                .ok_or_else(|| at(format!("`{ent}` is not an entity")))?;
            let fd = model
                .find_field(en, seg)
                .ok_or_else(|| at(format!("`{ent}` has no field `{seg}`")))?;
            if i.hidden(&ent, seg) {
                return Err((403, format!("filters, `{path}`: `{ent}.{seg}` is hidden")));
            }
            x = mk(ExprKind::Field {
                base: Box::new(x),
                name: seg.to_string(),
                optional: prev_opt,
            });
            prev_opt = prev_opt || matches!(fd.ty, Some(Ty::Optional(_)));
            if k + 1 < segs.len() {
                ent = fd
                    .ty
                    .as_ref()
                    .and_then(entity_named)
                    .filter(|t| {
                        model
                            .entity(t)
                            .is_some_and(|y| y.kind == EntityKind::Entity)
                    })
                    .ok_or_else(|| at(format!("`{seg}` is not a reference")))?;
            }
            last = fd.ty.clone();
        }
        let ty = match last {
            Some(Ty::Optional(t)) => *t,
            Some(t) => t,
            None => return Err(at("a field without a declared type".into())),
        };
        let tname = match &ty {
            Ty::Path { segments, .. } => segments.last().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        let bind = |v: V, vals: &mut Vec<(String, V)>| -> Expr {
            let name = format!("__fv{n}_{}", vals.len());
            vals.push((name.clone(), v));
            mk(ExprKind::Name(name))
        };
        let dec = |i: &mut Interp, j: &J| -> Result<V, Fail> {
            if j.is_null() {
                return Ok(V::None);
            }
            let fixed = match (&ty, j) {
                (Ty::Path { args, .. }, J::Number(x))
                    if matches!(tname.as_str(), "Money" | "Qty") && !args.is_empty() =>
                {
                    Some(json!(format!("{x} {}", quadra_ir::print::ty(&args[0]))))
                }
                (Ty::Path { args, .. }, J::String(s))
                    if matches!(tname.as_str(), "Money" | "Qty")
                        && !args.is_empty()
                        && s.trim().parse::<f64>().is_ok() =>
                {
                    Some(json!(format!(
                        "{} {}",
                        s.trim(),
                        quadra_ir::print::ty(&args[0])
                    )))
                }
                _ => None,
            };
            json::decode(i, model, Some(&ty), fixed.as_ref().unwrap_or(j), &|_| None)
                .map_err(|m| (400, format!("filters, `{path}`: {m}")))
        };
        let bin = |op: BinOp, l: Expr, r: Expr| {
            mk(ExprKind::Binary {
                op,
                lhs: Box::new(l),
                rhs: Box::new(r),
            })
        };
        let cond = match op {
            "eq" => {
                let v = dec(i, &f["value"])?;
                bin(BinOp::Eq, x, bind(v, &mut out.vals))
            }
            "in" => {
                let items = f["value"]
                    .as_array()
                    .ok_or_else(|| at("`in`: a list of values".into()))?;
                if items.len() > 500 {
                    return Err(at("`in`: at most 500 values".into()));
                }
                let mut vs = Vec::new();
                for it in items {
                    vs.push(dec(i, it)?);
                }
                bin(BinOp::In, x, bind(V::List(Rc::new(vs)), &mut out.vals))
            }
            "range" => {
                let r = f["value"]
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| at("`range`: [from, to], either may be null".into()))?;
                if matches!(tname.as_str(), "Date" | "DateTime") {
                    let day = |j: &J, end: bool| -> Result<chrono::NaiveDate, Fail> {
                        match j {
                            J::Null => Ok(if end {
                                chrono::NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()
                            } else {
                                chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap()
                            }),
                            J::String(s) => chrono::NaiveDate::parse_from_str(
                                s.get(..10).unwrap_or(s),
                                "%Y-%m-%d",
                            )
                            .map_err(|_| at(format!("`{s}` is not a date"))),
                            _ => Err(at("a date is text: YYYY-MM-DD".into())),
                        }
                    };
                    let (a, b) = (day(&r[0], false)?, day(&r[1], true)?);
                    bin(BinOp::In, x, bind(V::DateRange(a, b), &mut out.vals))
                } else if matches!(
                    tname.as_str(),
                    "Int" | "Decimal" | "Money" | "Qty" | "Percent" | "Price"
                ) {
                    let (lo, hi) = (dec(i, &r[0])?, dec(i, &r[1])?);
                    let mut parts = Vec::new();
                    if !lo.is_none() {
                        parts.push(bin(BinOp::Ge, x.clone(), bind(lo, &mut out.vals)));
                    }
                    if !hi.is_none() {
                        parts.push(bin(BinOp::Le, x.clone(), bind(hi, &mut out.vals)));
                    }
                    match parts.len() {
                        0 => continue,
                        1 => parts.pop().unwrap(),
                        _ => {
                            let b = parts.pop().unwrap();
                            bin(BinOp::And, parts.pop().unwrap(), b)
                        }
                    }
                } else {
                    return Err(at("`range` applies to dates and numbers".into()));
                }
            }
            "contains" => {
                if !quadra_runtime::query::text_type(Some(&ty)) {
                    return Err(at("`contains` applies to text".into()));
                }
                let t = f["value"].as_str().unwrap_or("").trim().to_string();
                if t.is_empty() {
                    continue;
                }
                if t.chars().count() > 200 {
                    return Err(at("at most 200 characters".into()));
                }
                let callee = mk(ExprKind::Field {
                    base: Box::new(x),
                    name: "contains".into(),
                    optional: false,
                });
                let arg = bind(V::Text(Rc::from(t.as_str())), &mut out.vals);
                mk(ExprKind::Call {
                    callee: Box::new(callee),
                    args: vec![Arg {
                        name: None,
                        value: arg,
                        spread: false,
                    }],
                })
            }
            "true" => bin(BinOp::Eq, x, bind(V::Bool(true), &mut out.vals)),
            _ => return Err(at(format!("`op` `{op}`: eq, in, range, contains or true"))),
        };
        out.exprs.push(cond);
    }
    Ok(out)
}
