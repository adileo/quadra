//! The Quadra compiler and engine in the browser: the sandbox of the website.
//!
//! Every function takes the project as JSON (`{"path": "text", …}`, paths relative to the
//! project) and answers JSON. Nothing is kept between calls: the sandbox sends the files it
//! has (they are small) and the engine checks, tests or replays them from scratch.
//!
//! The database of the sandbox is a *session*: an optional fixture and the statements typed
//! in its console, run as a hidden `test` block on the current code. Changing the code
//! replays the session, like a migration of the data: the records are always those the
//! current model would produce from the same history.

use quadra_runtime::{Interp, V};
use quadra_sema::{FileKind, Project, Severity};
use quadra_syntax::TextSize;
use serde_json::{json, Value as J};
use std::path::Path;
use wasm_bindgen::prelude::*;

const ROOT: &str = "/project";
const SESSION_FILE: &str = "tests/__session__.qd";
const SESSION_TEST: &str = "__session__";

fn project(files: &str) -> Result<Project, String> {
    let map: serde_json::Map<String, J> = serde_json::from_str(files).map_err(|e| format!("files: {e}"))?;
    let root = Path::new(ROOT);
    let sources = map
        .into_iter()
        .filter(|(p, _)| p.ends_with(".qd"))
        .map(|(p, t)| (root.join(p), t.as_str().unwrap_or("").to_string()))
        .collect();
    Ok(Project::from_sources(root, sources))
}

fn rel(project: &Project, path: &Path) -> String {
    path.strip_prefix(&project.root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

fn byte_at(text: &str, utf16_offset: usize) -> usize {
    let mut n = 0;
    for (i, c) in text.char_indices() {
        if n >= utf16_offset {
            return i;
        }
        n += c.len_utf16();
    }
    text.len()
}

fn diagnostics(project: &Project, diags: &[quadra_sema::Diagnostic]) -> Vec<J> {
    diags
        .iter()
        .filter_map(|d| {
            let f = project.file(d.file);
            if f.kind == FileKind::Std {
                return None;
            }
            let (from, to) = (usize::from(d.range.start()), usize::from(d.range.end()));
            let line = f.text[..from.min(f.text.len())].matches('\n').count() + 1;
            Some(json!({
                "path": rel(project, &f.path),
                "from": utf16(&f.text, from),
                "to": utf16(&f.text, to),
                "line": line,
                "severity": match d.severity { Severity::Error => "error", Severity::Warning => "warning", Severity::Hint => "hint" },
                "message": d.message,
            }))
        })
        .collect()
}

fn answer(r: Result<J, String>) -> String {
    match r {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// Names, types, cycles, flows: the compiler's messages, plus the fixtures and entities the
/// sandbox offers.
#[wasm_bindgen]
pub fn check(files: &str) -> String {
    answer((|| {
        let project = project(files)?;
        let checked = quadra_analysis::check(&project);
        let fixtures: Vec<J> = checked
            .model
            .as_ref()
            .map(|m| m.fixtures.iter().map(|f| json!({ "name": f.name, "module": project.file(f.span.file).module })).collect())
            .unwrap_or_default();
        let tests = checked.model.as_ref().map_or(0, |m| m.tests.len());
        Ok(json!({ "diagnostics": diagnostics(&project, &checked.diagnostics), "fixtures": fixtures, "tests": tests, "ok": checked.model.is_some() }))
    })())
}

/// The `test` blocks, each on a fresh in-memory database.
#[wasm_bindgen]
pub fn test(files: &str) -> String {
    answer((|| {
        let project = project(files)?;
        match quadra_runtime::run_tests(&project, None) {
            Ok(results) => Ok(json!({ "results": results.iter().filter(|r| r.name != SESSION_TEST).map(|r| json!({ "name": r.name, "location": r.location, "passed": r.passed, "message": r.message })).collect::<Vec<_>>() })),
            Err(diags) => Ok(json!({ "diagnostics": diagnostics(&project, &diags) })),
        }
    })())
}

/// What is under the cursor: hover text (markdown) and where it is defined.
#[wasm_bindgen]
pub fn ide(files: &str, path: &str, offset: usize) -> String {
    answer((|| {
        let project = project(files)?;
        let analysis = project.analyze();
        let abs = Path::new(ROOT).join(path);
        let Some(f) = project.files().find(|f| f.path == abs) else { return Err(format!("{path}: not a file of the project")) };
        let at = TextSize::from(byte_at(&f.text, offset) as u32);
        let hover = quadra_sema::ide::hover(&project, &analysis, f.id, at).map(|(range, md)| json!({ "from": utf16(&f.text, usize::from(range.start())), "to": utf16(&f.text, usize::from(range.end())), "markdown": md }));
        let definition = quadra_sema::ide::definition(&project, &analysis, f.id, at).and_then(|t| {
            let tf = project.file(t.file);
            (tf.kind != FileKind::Std).then(|| json!({ "path": rel(&project, &tf.path), "from": utf16(&tf.text, usize::from(t.range.start())), "to": utf16(&tf.text, usize::from(t.range.end())) }))
        });
        Ok(json!({ "hover": hover, "definition": definition }))
    })())
}

/// The test block of a session: `use` the fixture, then the statements, each starting on a
/// known line (to say which one failed).
fn session_source(fixture: Option<(&str, &str)>, statements: &[String]) -> (String, Vec<usize>) {
    let mut src = String::new();
    if let Some((module, name)) = fixture {
        src.push_str(&format!("from {module} import {name}\n\n"));
    }
    src.push_str(&format!("test \"{SESSION_TEST}\":\n"));
    let mut line = src.matches('\n').count() + 1;
    if let Some((_, name)) = fixture {
        src.push_str(&format!("    use {name}\n"));
        line += 1;
    }
    let mut starts = Vec::new();
    for s in statements {
        starts.push(line);
        let lines: Vec<&str> = s.trim_end().lines().collect();
        // several writes made together (a form saved in the app): one transaction
        let together = lines.len() > 1 && lines.iter().all(|l| !l.starts_with(' ') && !l.starts_with("let ") && !l.ends_with(':'));
        let indent = if together {
            src.push_str("    if true:\n");
            line += 1;
            "        "
        } else {
            "    "
        };
        for l in lines {
            src.push_str(indent);
            src.push_str(l);
            src.push('\n');
            line += 1;
        }
    }
    if statements.is_empty() && fixture.is_none() {
        src.push_str("    let __empty = 0\n");
    }
    (src, starts)
}

fn display(i: &mut Interp, v: &V) -> J {
    match v {
        V::None => J::Null,
        V::Bool(b) => json!(b),
        V::Ref { ent, id } => json!({ "ref": id, "ent": &**ent, "d": i.display(v) }),
        other => json!(i.display(other)),
    }
}

/// The sandbox database: run the session on the current code and return every record.
/// `fixture` is `module.name` or empty; `statements` a JSON list of Quadra statements.
#[wasm_bindgen]
pub fn session(files: &str, fixture: &str, statements: &str) -> String {
    answer((|| {
        let statements: Vec<String> = serde_json::from_str(statements).map_err(|e| format!("statements: {e}"))?;
        let fixture = fixture.rsplit_once('.').filter(|(m, n)| !m.is_empty() && !n.is_empty());
        let (src, starts) = session_source(fixture, &statements);
        let mut map: serde_json::Map<String, J> = serde_json::from_str(files).map_err(|e| format!("files: {e}"))?;
        map.insert(SESSION_FILE.into(), json!(src));
        let project = project(&J::Object(map).to_string())?;
        let result = quadra_runtime::run_session(&project, SESSION_TEST, |model, _, i, _| {
            let mut entities = Vec::new();
            for e in model.entities.iter().filter(|e| e.kind == quadra_ir::EntityKind::Entity && !e.temporary && !e.is_ledger) {
                let fields: Vec<String> = model.all_fields(e).iter().filter(|f| !f.name.starts_with("__")).map(|f| f.name.clone()).collect();
                let ids = i.ids(&e.name, None);
                let mut rows = Vec::new();
                for id in ids.iter().take(500) {
                    let rec = V::Ref { ent: std::rc::Rc::from(e.name.as_str()), id: *id };
                    let cells: Vec<J> = fields
                        .iter()
                        .map(|f| match i.get(&rec, f, None, None) {
                            Ok(v) => display(i, &v),
                            Err(err) => json!({ "error": format!("{err:?}").chars().take(200).collect::<String>() }),
                        })
                        .collect();
                    rows.push(json!({ "id": id, "display": i.display(&rec), "cells": cells }));
                }
                let module = project.file(e.span.file).module.clone();
                entities.push(json!({ "name": e.name, "module": module, "fields": fields, "count": ids.len(), "rows": rows }));
            }
            entities
        });
        match result {
            Err(diags) => {
                // errors in the session itself point to the statement that caused them
                let in_session: Vec<J> = diagnostics(&project, &diags).into_iter().filter(|d| d["path"] == json!(SESSION_FILE)).collect();
                let failed_at = in_session.first().and_then(|d| d["line"].as_u64()).map(|l| starts.iter().rposition(|s| *s as u64 <= l).unwrap_or(0));
                Ok(json!({ "diagnostics": diagnostics(&project, &diags), "failed_at": failed_at, "compile_error": true }))
            }
            Ok((failure, entities)) => {
                let failed_at = failure.as_ref().and_then(|m| {
                    let i = m.find("__session__.qd:")? + "__session__.qd:".len();
                    let line: usize = m[i..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
                    starts.iter().rposition(|s| *s <= line)
                });
                Ok(json!({ "error_message": failure, "failed_at": failed_at, "entities": entities }))
            }
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
        let p = Project::load(&root).unwrap();
        let mut m = serde_json::Map::new();
        for f in p.user_files() {
            m.insert(f.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned(), json!(f.text));
        }
        J::Object(m).to_string()
    }

    #[test]
    fn checks_tests_and_replays_a_session() {
        let fs = files();
        let c: J = serde_json::from_str(&check(&fs)).unwrap();
        assert_eq!(c["ok"], json!(true), "{c}");
        assert!(c["fixtures"].as_array().unwrap().iter().any(|f| f["name"] == json!("italy")), "{c}");
        let t: J = serde_json::from_str(&test(&fs)).unwrap();
        assert!(t["results"].as_array().unwrap().iter().all(|r| r["passed"] == json!(true)), "{t}");
        let s: J = serde_json::from_str(&session(&fs, "tests.fixtures.italy", &json!(["let so = SalesOrder(customer=rossi, lines=[SalesLine(product=widget, qty=3 pcs)])", "so.confirm()"]).to_string())).unwrap();
        assert_eq!(s["error_message"], J::Null, "{}", s["error_message"]);
        let so = s["entities"].as_array().unwrap().iter().find(|e| e["name"] == json!("SalesOrder")).unwrap();
        assert_eq!(so["count"], json!(1));
        // a failing statement is pointed at
        let s: J = serde_json::from_str(&session(&fs, "tests.fixtures.italy", &json!(["let a = 1", "fail \"no\"", "let b = 2"]).to_string())).unwrap();
        assert_eq!(s["failed_at"], json!(1), "{s}");
        // an unknown name: a compile error in the session, on its statement
        let s: J = serde_json::from_str(&session(&fs, "", &json!(["let a = 1", "let b = Nope()"]).to_string())).unwrap();
        assert_eq!((s["compile_error"].clone(), s["failed_at"].clone()), (json!(true), json!(1)), "{s}");
    }
}

// ---- the API of the web app, on the session ---------------------------------------------------

mod app;

/// The HTTP API of the server (`crates/quadra-api`), answered on the session: the real web
/// app runs on it inside the sandbox. `request` is `{method, path, body}`; the answer is
/// `{status, body}`, plus `append` (a statement for the session) after a successful write.
#[wasm_bindgen]
pub fn app(files: &str, fixture: &str, statements: &str, request: &str) -> String {
    app::handle(files, fixture, statements, request).to_string()
}
