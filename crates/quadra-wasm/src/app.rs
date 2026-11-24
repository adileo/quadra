//! The server's HTTP API answered on the sandbox session, for the web app running inside the
//! sandbox. Reads use the same functions as the server (`quadra-api`). A write is applied to
//! the session's data exactly as the server would apply it, then written down as Quadra
//! statements appended to the session: the data of the app, like the console's, is replayed
//! on every change of the code and goes into the downloaded project.

use super::*;
use quadra_api::json::{decode, describe};
use quadra_api::{apply_ops, model_json, overlay_source, query_items, query_rows, record_json};
use quadra_ir::{EntityKind, Model};
use quadra_runtime::{Interp, V};
use quadra_types::Report;
use std::collections::HashMap;
use std::rc::Rc;

fn answer(status: u16, body: J) -> J {
    json!({ "status": status, "body": body })
}

fn error(status: u16, msg: impl Into<String>) -> J {
    answer(status, json!({ "error": msg.into() }))
}

type Inspect<'a, T> = &'a mut dyn FnMut(&Project, &Model, &Report, &mut Interp, &HashMap<String, V>) -> T;

/// Run the session (plus `extra` files) on the code and look at its final state. Errors: the
/// diagnostics, when the code does not compile.
fn on_session<T>(files: &str, extra: &[(&str, String)], fixture: &str, statements: &[String], inspect: Inspect<T>) -> Result<(Option<String>, T), Vec<J>> {
    let fixture = fixture.rsplit_once('.').filter(|(m, n)| !m.is_empty() && !n.is_empty());
    let (src, _) = session_source(fixture, statements);
    let mut map: serde_json::Map<String, J> = serde_json::from_str(files).unwrap_or_default();
    map.insert(SESSION_FILE.into(), json!(src));
    for (p, t) in extra {
        map.insert((*p).into(), json!(t));
    }
    let project = project(&J::Object(map).to_string()).map_err(|e| vec![json!({ "message": e })])?;
    quadra_runtime::run_session(&project, SESSION_TEST, |model, report, i, bindings| {
        if i.company.is_none() {
            i.company = i.ids("Company", None).first().copied();
        }
        inspect(&project, model, report, i, bindings)
    })
    .map_err(|diags| diagnostics(&project, &diags))
}

fn code_errors(diags: &[J]) -> J {
    let first = diags.iter().find(|d| d["severity"] == json!("error")).and_then(|d| d["message"].as_str()).unwrap_or("the code does not compile");
    error(503, format!("The code has errors — fix them in the editor: {first}"))
}

pub fn handle(files: &str, fixture: &str, statements: &str, request: &str) -> J {
    let Ok(statements) = serde_json::from_str::<Vec<String>>(statements) else { return error(400, "statements") };
    let Ok(req) = serde_json::from_str::<J>(request) else { return error(400, "request") };
    let method = req["method"].as_str().unwrap_or("GET").to_uppercase();
    let full = req["path"].as_str().unwrap_or("");
    let path = full.split('?').next().unwrap_or("");
    let body = &req["body"];
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segs.as_slice()) {
        ("GET", ["v1", "me"]) => answer(200, json!({ "user": "sandbox", "user_id": 0, "read_only": false, "csrf": "sandbox", "kind": "session" })),
        ("GET", ["v1", "model"]) => {
            match on_session(files, &[], fixture, &statements, &mut |project, model, report, i, _| {
                let entities = model_json(i, model, report, &|s| project.file(s.file).module.clone());
                json!({ "entities": entities, "base": i.store.base })
            }) {
                Ok((_, b)) => answer(200, b),
                Err(d) => code_errors(&d),
            }
        }
        ("POST", ["v1", "query"]) => {
            let (entity, items, columns) = match query_items(body) {
                Ok(x) => x,
                Err((s, m)) => return error(s, m),
            };
            let extra: Vec<(&str, String)> = if items.is_empty() { vec![] } else { vec![("tests/__api__.qd", overlay_source(&entity, &items))] };
            match on_session(files, &extra, fixture, &statements, &mut |_, model, _, i, _| {
                if model.entity(&entity).map_or(true, |e| e.kind != EntityKind::Entity || e.temporary) {
                    return Err((404, format!("unknown entity `{entity}`")));
                }
                query_rows(i, model, &entity, body, &items, &columns)
            }) {
                Ok((_, Ok(b))) => answer(200, b),
                Ok((_, Err((s, m)))) => error(s, m),
                Err(d) if d.iter().any(|x| x["path"] == json!("tests/__api__.qd")) => {
                    let msgs: Vec<&J> = d.iter().filter(|x| x["path"] == json!("tests/__api__.qd")).map(|x| &x["message"]).collect();
                    answer(400, json!({ "error": "the expression does not check", "diagnostics": msgs }))
                }
                Err(d) => code_errors(&d),
            }
        }
        ("GET", ["v1", "search", entity]) => {
            // the query string: q, limit, after (percent-encoded)
            let query = full.split_once('?').map(|(_, q)| q).unwrap_or("");
            let param = |k: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(a, _)| *a == k).map(|(_, v)| percent_decode(v)));
            let text = param("q").unwrap_or_default();
            let limit = param("limit").and_then(|v| v.parse().ok()).unwrap_or(20);
            let after = param("after");
            match on_session(files, &[], fixture, &statements, &mut |_, model, _, i, _| quadra_api::search_rows(i, model, entity, &text, limit, after.as_deref())) {
                Ok((_, Ok(b))) => answer(200, b),
                Ok((_, Err((s, m)))) => error(s, m),
                Err(d) => code_errors(&d),
            }
        }
        ("GET", ["v1", "records", entity, id, "history"]) => answer(200, json!({ "entity": entity, "id": id.parse::<u64>().unwrap_or(0), "entries": [] })),
        ("GET", ["v1", "records", entity, id]) => {
            let Ok(id) = id.parse::<u64>() else { return error(400, "id") };
            match on_session(files, &[], fixture, &statements, &mut |_, model, _, i, _| record_json(i, model, entity, id)) {
                Ok((_, Ok(b))) => answer(200, b),
                Ok((_, Err((s, m)))) => error(s, m),
                Err(d) => code_errors(&d),
            }
        }
        ("POST", ["v1", "tx"]) => write(files, fixture, &statements, body),
        ("POST", ["v1", "auth", _]) => answer(200, json!({ "user": "sandbox", "csrf": "sandbox" })),
        _ => error(404, format!("{method} {path}: not available in the sandbox")),
    }
}

/// `%41` → `A`, `+` → space.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(x) => {
                        out.push(x);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A write of the app: applied to the data as the server would, then written down as Quadra.
fn write(files: &str, fixture: &str, statements: &[String], body: &J) -> J {
    let Some(ops) = body["ops"].as_array() else { return error(400, "`ops` is required") };
    if ops.is_empty() || ops.len() > 200 {
        return error(400, "1 to 200 operations per transaction");
    }
    // every created record gets a name, to refer to it in the statements
    let ops: Vec<J> = ops
        .iter()
        .enumerate()
        .map(|(n, op)| {
            let mut op = op.clone();
            if op["op"] == json!("create") && op["ref"].as_str().is_none() {
                op["ref"] = json!(format!("__op{n}"));
            }
            op
        })
        .collect();
    let written = on_session(files, &[], fixture, statements, &mut |_, model, _, i, bindings| -> Result<(String, serde_json::Map<String, J>), String> {
        let created = i.transaction(|i| apply_ops(i, model, &ops)).map_err(describe)?;
        let mut names: HashMap<u64, String> = HashMap::new();
        let mut taken: Vec<String> = bindings.keys().cloned().collect();
        for (n, v) in bindings {
            if let V::Ref { id, .. } = v {
                let better = names.get(id).map_or(true, |old| old.starts_with("__") || (n.len() < old.len() && !n.starts_with("__")));
                if better {
                    names.insert(*id, n.clone());
                }
            }
        }
        let mut lines = Vec::new();
        for op in &ops {
            let ent = op["entity"].as_str().unwrap_or("");
            let empty = serde_json::Map::new();
            let values = op["values"].as_object().unwrap_or(&empty);
            match op["op"].as_str() {
                Some("create") => {
                    let id = created.get(op["ref"].as_str().unwrap_or("")).and_then(|x| x.as_u64()).ok_or("created record not found")?;
                    let var = fresh(&snake(ent), &taken, statements);
                    taken.push(var.clone());
                    let ctor = constructor(i, model, ent, id, values, &names)?;
                    names.insert(id, var.clone());
                    lines.push(format!("let {var} = {ctor}"));
                }
                Some("update") => {
                    let id = op["id"].as_u64().ok_or("`id` is required")?;
                    let target = reference(ent, id, &names);
                    let rec = V::Ref { ent: Rc::from(ent), id };
                    for k in values.keys() {
                        let v = i.get(&rec, k, i.company, None).map_err(describe)?;
                        lines.push(format!("{target}.{k} = {}", literal(i, model, &v, &names)?));
                    }
                }
                Some("action") => {
                    let id = op["id"].as_u64().ok_or("`id` is required")?;
                    let name = op["action"].as_str().ok_or("`action` is required")?;
                    let target = reference(ent, id, &names);
                    let mut args = Vec::new();
                    if let Some(given) = op["args"].as_object() {
                        let params = model.entity(ent).and_then(|e| e.actions.iter().find(|a| a.name == name)).map(|a| a.params.clone()).unwrap_or_default();
                        for (k, v) in given {
                            let ty = params.iter().find(|p| &p.name == k).and_then(|p| p.ty.clone());
                            let val = decode(i, model, ty.as_ref(), v, &|_| None)?;
                            args.push(format!("{k}={}", literal(i, model, &val, &names)?));
                        }
                    }
                    lines.push(format!("{target}.{name}({})", args.join(", ")));
                }
                _ => return Err("`op` must be create, update or action".into()),
            }
        }
        let public: serde_json::Map<String, J> = created.into_iter().filter(|(k, _)| !k.starts_with("__op")).collect();
        Ok((lines.join("\n"), public))
    });
    let (entry, created) = match written {
        Ok((_, Ok(x))) => x,
        Ok((_, Err(m))) => return error(422, m),
        Err(d) => return code_errors(&d),
    };
    // the statement must replay: run the session with it
    let mut next = statements.to_vec();
    next.push(entry.clone());
    match on_session(files, &[], fixture, &next, &mut |_, _, _, i, _| i.store.tx) {
        Ok((None, tx)) => json!({ "status": 200, "body": { "tx": tx, "base": tx, "created": created }, "append": entry }),
        Ok((Some(m), _)) => error(422, format!("{} (as Quadra: {entry})", m.replace(&format!("{SESSION_FILE}:"), "line "))),
        Err(d) => error(422, format!("{} (as Quadra: {entry})", d.first().and_then(|x| x["message"].as_str()).unwrap_or("does not compile"))),
    }
}

fn snake(ent: &str) -> String {
    let mut out = String::new();
    for (k, c) in ent.chars().enumerate() {
        if c.is_uppercase() && k > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// `invoice1`, `invoice2`…: a name not used by the session.
fn fresh(base: &str, taken: &[String], statements: &[String]) -> String {
    (1..)
        .map(|n| format!("{base}{n}"))
        .find(|n| !taken.contains(n) && !statements.iter().any(|s| s.contains(&format!("let {n} "))))
        .unwrap()
}

/// A record in a statement: its variable in the session, else by id.
fn reference(ent: &str, id: u64, names: &HashMap<u64, String>) -> String {
    match names.get(&id) {
        Some(n) => n.clone(),
        None => format!("{ent}.single(r => r.id == \"{id}\")"),
    }
}

/// `Entity(field=value, …)` with the values the record got (defaults and conversions
/// included), lines written inline.
fn constructor(i: &mut Interp, model: &Model, ent: &str, id: u64, values: &serde_json::Map<String, J>, names: &HashMap<u64, String>) -> Result<String, String> {
    let rec = V::Ref { ent: Rc::from(ent), id };
    let mut args = Vec::new();
    for (k, j) in values {
        let v = i.get(&rec, k, i.company, None).map_err(describe)?;
        let text = match (&v, j.as_array()) {
            (V::List(items), Some(objs)) if objs.iter().all(|o| o.is_object()) => {
                let mut parts = Vec::new();
                for (item, obj) in items.iter().zip(objs) {
                    let V::Ref { ent: ce, id: cid } = item else { return Err(format!("{ent}.{k}: lines expected")) };
                    let mut o = obj.as_object().cloned().unwrap_or_default();
                    // the line's document is set by writing it inside the document
                    if let Some(e) = model.entity(ce) {
                        o.retain(|name, _| e.fields.iter().find(|f| &f.name == name).map_or(true, |f| !f.parent));
                    }
                    parts.push(constructor(i, model, ce, *cid, &o, names)?);
                }
                format!("[{}]", parts.join(", "))
            }
            _ => literal(i, model, &v, names)?,
        };
        args.push(format!("{k}={text}"));
    }
    Ok(format!("{ent}({})", args.join(", ")))
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '{' => out.push_str("{"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value as Quadra source.
fn literal(i: &mut Interp, model: &Model, v: &V, names: &HashMap<u64, String>) -> Result<String, String> {
    let list = |i: &mut Interp, xs: &[V], open: &str, close: &str| -> Result<String, String> {
        let parts: Result<Vec<String>, String> = xs.iter().map(|x| literal(i, model, x, names)).collect();
        Ok(format!("{open}{}{close}", parts?.join(", ")))
    };
    Ok(match v {
        V::None => "none".into(),
        V::Bool(b) => b.to_string(),
        V::Int(n) => n.to_string(),
        V::Dec(d) => d.normalize().to_string(),
        V::Pct(d) => format!("{}%", (d * rust_decimal::Decimal::from(100)).normalize()),
        V::Text(t) => quote(t),
        V::Date(d) => d.format("%Y-%m-%d").to_string(),
        V::DateTime(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        V::YearMonth(y, m) => format!("{y:04}-{m:02}"),
        V::Duration(s) if s % 86400 == 0 => format!("{} days", s / 86400),
        V::Duration(s) if s % 3600 == 0 => format!("{} hours", s / 3600),
        V::Duration(s) => format!("{} minutes", s / 60),
        V::Money { amt, cur } => format!("{amt} {cur}"),
        V::Price { amt, cur, per } => format!("{amt} {cur}/{per}"),
        V::Qty { v, unit } => format!("{v} {unit}"),
        V::Ref { ent, id } => reference(ent, *id, names),
        V::Rec { ent, fields } => {
            let parts: Result<Vec<String>, String> = fields.iter().map(|(k, x)| Ok(format!("{k}={}", literal(i, model, x, names)?))).collect();
            format!("{ent}({})", parts?.join(", "))
        }
        V::Variant { name, fields } if fields.is_empty() => name.to_string(),
        V::Variant { name, fields } => list(i, fields, &format!("{name}("), ")")?,
        V::List(xs) => list(i, xs, "[", "]")?,
        V::Set(xs) => list(i, xs, "{", "}")?,
        V::Role(r) => r.to_string(),
        other => return Err(format!("cannot write {other} as Quadra")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn welcome() -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/welcome");
        let p = Project::load(&root).unwrap();
        let mut m = serde_json::Map::new();
        for f in p.user_files() {
            m.insert(f.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned(), json!(f.text));
        }
        J::Object(m).to_string()
    }

    fn call(fs: &str, statements: &[String], method: &str, path: &str, body: J) -> J {
        let req = json!({ "method": method, "path": path, "body": body }).to_string();
        handle(fs, "tests.billing.test.demo", &json!(statements).to_string(), &req)
    }

    #[test]
    fn the_web_app_api_runs_on_the_session() {
        let fs = welcome();
        let model = call(&fs, &[], "GET", "/v1/model", J::Null);
        assert_eq!(model["status"], json!(200), "{model}");
        assert!(model["body"]["entities"].as_array().unwrap().iter().any(|e| e["name"] == json!("Invoice")));
        let q = call(&fs, &[], "POST", "/v1/query", json!({ "entity": "Customer", "columns": { "big": "credit_limit > 2000.00 EUR" } }));
        assert_eq!(q["body"]["total"], json!(2), "{q}");
        let acme = q["body"]["rows"].as_array().unwrap().iter().find(|r| r["display"] == json!("ACME Inc.")).unwrap()["id"].as_u64().unwrap();
        // a new invoice with a line, from the app: a statement of the session
        let tx = call(&fs, &[], "POST", "/v1/tx", json!({ "ops": [{ "op": "create", "entity": "Invoice", "ref": "x", "values": { "customer": acme, "lines": [{ "description": "Consulting", "qty": "2", "price": "500 EUR" }] } }] }));
        assert_eq!(tx["status"], json!(200), "{tx}");
        let entry = tx["append"].as_str().unwrap().to_string();
        assert!(entry.starts_with("let invoice1 = Invoice(customer=acme, lines=[InvoiceLine("), "{entry}");
        let id = tx["body"]["created"]["x"].as_u64().unwrap();
        // the record, then an action on it: refers to the variable
        let s = vec![entry];
        let r = call(&fs, &s, "GET", &format!("/v1/records/Invoice/{id}"), J::Null);
        assert_eq!(r["status"], json!(200), "{r}");
        assert_eq!(r["body"]["cells"]["total"]["v"], json!({ "money": "1000", "cur": "EUR" }), "{r}");
        let post = call(&fs, &s, "POST", "/v1/tx", json!({ "base": 1, "ops": [{ "op": "action", "entity": "Invoice", "id": id, "action": "post" }] }));
        assert_eq!(post["append"], json!("invoice1.post()"), "{post}");
        // an update refused by a rule: nothing appended, the model's message
        let over = call(&fs, &s, "POST", "/v1/tx", json!({ "base": 1, "ops": [{ "op": "update", "entity": "Customer", "id": acme, "values": { "credit_limit": "10 EUR" } }] }));
        assert_eq!(over["status"], json!(422), "{over}");
        assert!(over["append"].is_null());
        let ok = call(&fs, &s, "POST", "/v1/tx", json!({ "base": 1, "ops": [{ "op": "update", "entity": "Customer", "id": acme, "values": { "name": "ACME \"Global\"" } }] }));
        assert_eq!(ok["append"], json!("acme.name = \"ACME \\\"Global\\\"\""), "{ok}");
    }
}
