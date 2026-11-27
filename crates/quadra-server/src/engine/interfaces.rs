//! Apps and views from the project's interface files (`interfaces/**/*.qdi`, the interface files reference (quadra.run/reference/interfaces)),
//! and the custom forms of apps.
//!
//! - **Install**: when the server starts and when `quadra dev` applies a new version, the
//!   files' apps and views become rows of `_quadra_views` (`origin = file`, `path` their
//!   identity: `sales/cockpit`, `sales/orders#To ship`); the folders of apps are the
//!   directories of their files (store::install_files).
//! - **Read-only in production**: an app or a view of a file is changed in its file (and
//!   deployed); the API refuses to change it, says which file it is defined in, and
//!   duplicating it makes an app of one's own. Shares with users and teams are data: they
//!   are added on top of the file's (roles, everyone).
//! - **Written back in `quadra dev`**: a change made in the client is saved as usual and the
//!   file is written again, in canonical form, atomically (a new file then renamed).
//! - **Custom forms** (`POST /v1/apps/{id}/forms/{component}/submit`): the statements of a
//!   form of the stored app run on the server as the viewer, in one transaction, with the
//!   viewer's permissions; the client sends only the values of the fields and of the app's
//!   parameters.

use super::views::{at_now, bad, Who};
use super::*;
use quadra_ir::StmtKind;
use quadra_runtime::Ctx;
use quadra_store::views::{self as store, FileItem, Share, ViewRow};
use std::path::Path;

/// The file of an app or a view of a file: `interfaces/sales/cockpit.qdi`.
pub fn file_label(v: &ViewRow) -> Option<String> {
    (v.origin.as_deref() == Some("file")).then(|| {
        format!(
            "interfaces/{}.qdi",
            store::file_of(v.path.as_deref().unwrap_or(""))
        )
    })
}

/// The items of a project's interface files, those that convert (the others are reported
/// by the checker and keep what was installed of them).
fn file_items(root: &Path, project: &Project) -> (Vec<FileItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let base = root.join(quadra_sema::INTERFACES_DIR);
    for f in project.interface_files() {
        let rel = f
            .path
            .strip_prefix(&base)
            .unwrap_or(&f.path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let defs = match quadra_qdi::to_json(&f.text) {
            Ok(d) => d,
            Err(_) => {
                skipped.push(rel);
                continue;
            }
        };
        let dirs: Vec<String> = rel
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .map(|(_, d)| d.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        for d in defs {
            let app = d["kind"] == json!("app");
            let path = if app {
                rel.clone()
            } else {
                format!("{rel}#{}", d["name"].as_str().unwrap_or(""))
            };
            items.push(FileItem {
                path,
                dirs: if app { dirs.clone() } else { Vec::new() },
                def: d,
            });
        }
    }
    (items, skipped)
}

impl Engine {
    pub(super) fn dev_mode(&self) -> bool {
        self.watch
    }

    /// Install the apps and views of the project's interface files.
    pub(super) fn install_interfaces(&mut self, project: &Project) -> Result<(), String> {
        let (items, skipped) = file_items(&self.root, project);
        if !skipped.is_empty() {
            // a file that does not convert keeps what was installed of it
            return Err(format!(
                "interface files with errors: {}",
                skipped.join(", ")
            ));
        }
        let key = self.model_key();
        let done = self.side(move |c| store::install_files(c, &items, &at_now(), Some(&key)))?;
        if done.created + done.changed + done.removed > 0 {
            self.installed_any = true;
            eprintln!(
                "interfaces: {} created, {} changed, {} removed",
                done.created, done.changed, done.removed
            );
        }
        Ok(())
    }

    /// May the caller change this app or view (or folder) of a file? Not in production.
    pub(super) fn file_guard(&self, v: &ViewRow) -> Result<(), Fail> {
        match file_label(v) {
            Some(f) if !self.dev_mode() => Err((409, format!("defined in {f}: change the file and deploy it, or duplicate it to make an app of your own"))),
            Some(_) if v.kind == "folder" => Err((409, "a folder of interfaces/ is a directory: rename, move or delete it on disk".into())),
            _ => Ok(()),
        }
    }

    /// In production, may these shares replace those of a file's app or view? Only the
    /// shares with users and teams change (roles and everyone are the file's).
    pub(super) fn file_shares_ok(
        &self,
        v: &ViewRow,
        old: &[Share],
        new: &[Share],
    ) -> Result<(), Fail> {
        if v.origin.as_deref() != Some("file") || self.dev_mode() {
            return Ok(());
        }
        let file = |s: &[Share]| -> Vec<Share> {
            let mut x: Vec<Share> = s
                .iter()
                .filter(|s| !(s.target.starts_with("user:") || s.target.starts_with("team:")))
                .cloned()
                .collect();
            x.sort();
            x
        };
        if file(old) != file(new) {
            return Err((409, format!("defined in {}: the roles and everyone it is shared with are written in the file; share it here with users and teams", file_label(v).unwrap_or_default())));
        }
        Ok(())
    }

    /// `quadra dev`: write the file of an app or view again from what the database holds,
    /// canonical, atomically. A file left with nothing is removed.
    pub(super) fn write_back(&mut self, file: &str) -> Result<(), Fail> {
        let rows = store::file_rows(&mut self.conn, file).map_err(|e| (500, e))?;
        let ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
        let shares = store::shares(&mut self.conn, Some(&ids)).map_err(|e| (500, e))?;
        let mut rows = rows;
        // the app first, then the views in the order they were installed
        rows.sort_by_key(|r| (r.kind != "app", r.id));
        let defs: Vec<J> = rows
            .iter()
            .map(|r| definition(r, shares.get(&r.id).map(Vec::as_slice).unwrap_or(&[])))
            .collect();
        let path = self
            .root
            .join(quadra_sema::INTERFACES_DIR)
            .join(format!("{file}.qdi"));
        if defs.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let (text, _) =
            quadra_qdi::from_json(&defs).map_err(|e| (500, format!("{file}.qdi: {e}")))?;
        write_atomically(&path, &text).map_err(|e| (500, format!("{}: {e}", path.display())))?;
        self.seen_interfaces = None;
        Ok(())
    }

    /// `quadra dev`: an app of a file moved to another folder: its file moves to the folder's
    /// directory (made as needed).
    pub(super) fn move_file_app(
        &mut self,
        _id: u64,
        path: &str,
        parent: Option<u64>,
    ) -> Result<(), Fail> {
        let file = store::file_of(path).to_string();
        let base = file.rsplit('/').next().unwrap_or(&file).to_string();
        let mut chain: Vec<String> = Vec::new();
        let mut cur = parent;
        let mut guard = 0;
        while let Some(id) = cur {
            let f = store::view(&mut self.conn, id)
                .map_err(|e| (500, e))?
                .ok_or((404, format!("folder {id} not found")))?;
            chain.push(f.name.clone());
            cur = f.parent;
            guard += 1;
            if guard > 20 {
                break;
            }
        }
        chain.reverse();
        chain.push(base);
        let to = chain.join("/");
        if to == file {
            return self.write_back(&file);
        }
        let dir = self.root.join(quadra_sema::INTERFACES_DIR);
        if dir.join(format!("{to}.qdi")).exists() {
            return Err((
                409,
                format!("interfaces/{to}.qdi exists already: rename one of the apps first"),
            ));
        }
        let (from, into) = (file.clone(), to.clone());
        self.side(move |c| store::move_file(c, &from, &into))
            .map_err(|e| (500, e))?;
        self.write_back(&to)?;
        let _ = std::fs::remove_file(dir.join(format!("{file}.qdi")));
        Ok(())
    }

    /// `GET /v1/views/{id}/qdi`: an app or a saved view as a `.qdi` file (what `quadra apps
    /// pull` writes; "Export as .qdi" downloads it).
    pub(super) fn view_qdi(&mut self, who: &Who, id: u64) -> Result<Reply, Fail> {
        let (v, shares, _) = self.visible(who, id)?;
        if v.kind == "folder" {
            return Err(bad(
                "a folder is a directory of interfaces/: export its apps",
            ));
        }
        let def = definition(&v, &shares);
        let (text, warnings) = quadra_qdi::from_json(&[def]).map_err(|e| (500, e))?;
        let name = file_name(&v.name);
        Ok(reply(
            200,
            json!({ "file": format!("{name}.qdi"), "text": text, "warnings": warnings, "defined_in": file_label(&v) }),
        ))
    }

    // ---- custom forms -------------------------------------------------------------------------

    /// `POST /v1/apps/{id}/forms/{component}/submit`: `{values: {field: value}, params: [...],
    /// idempotency_key}`. The statements of the form of the stored app run as the caller.
    pub(super) fn form_submit(
        &mut self,
        p: &Principal,
        who: &Who,
        app_id: u64,
        comp_id: &str,
        body: &J,
    ) -> Result<Reply, Fail> {
        let (app, _, _) = self.visible(who, app_id)?;
        if app.kind != "app" {
            return Err((404, format!("app {app_id} not found")));
        }
        if let Some(v) = body["version"].as_u64() {
            if v != app.version {
                return Ok(reply(
                    409,
                    json!({ "error": "the app changed since you opened it: open it again", "version": app.version }),
                ));
            }
        }
        let comp = app.config["pages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|pg| pg["components"].as_array().cloned().unwrap_or_default())
            .find(|c| c["id"] == json!(comp_id) && c["type"] == json!("form"))
            .ok_or((404, format!("no form `{comp_id}` in app {app_id}")))?;
        let cfg = &comp["config"];
        if cfg["mode"] != json!("custom") {
            return Err(bad(format!(
                "form `{comp_id}` is not a custom form: it creates or edits records with `/v1/tx`"
            )));
        }
        let key = body["idempotency_key"].as_str().unwrap_or("").to_string();
        if key.is_empty()
            || key.len() > 100
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c))
        {
            return Err(bad(
                "`idempotency_key`: 1 to 100 characters among letters, digits, - _ : .",
            ));
        }
        let fields: Vec<J> = cfg["fields"].as_array().cloned().unwrap_or_default();
        let code = cfg["submit"].as_str().unwrap_or("").to_string();
        // the function: the app's parameters, the fields, the `where`s, the statements
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let scope = json!({ "params": body["params"] });
        let params = {
            let mut i = self.interp(model, Some(p), None)?;
            super::params::params_of(&mut i, model, &scope)?
        };
        let lets = lets_of(&app.config);
        let mut src = String::from("fn __form(\n");
        for prm in &params.list {
            match &prm.ty {
                Some(t) => src.push_str(&format!("    {}: {t},\n", prm.var)),
                None => src.push_str(&format!("    {},\n", prm.var)),
            }
        }
        let mut names = Vec::new();
        for f in &fields {
            let (n, t) = (
                f["name"].as_str().unwrap_or(""),
                f["type"].as_str().unwrap_or(""),
            );
            if !quadra_qdi::text::is_ident(n) || t.is_empty() || t.contains('\n') {
                return Err((
                    500,
                    format!("form `{comp_id}`: field `{n}` has no valid name or type"),
                ));
            }
            names.push(n.to_string());
            src.push_str(&format!("    {n}: {t},\n"));
        }
        src.push_str("):\n");
        // `where` of a field: `> 0`, `0..10`
        let mut wheres: Vec<(String, String)> = Vec::new();
        for f in &fields {
            let (n, w) = (
                f["name"].as_str().unwrap_or(""),
                f["where"].as_str().unwrap_or("").trim(),
            );
            if w.is_empty() {
                continue;
            }
            let op = ["==", "!=", "<=", ">=", "<", ">"]
                .iter()
                .any(|o| w.starts_with(o));
            let cond = if op {
                format!("{n} {w}")
            } else if w.contains("..") {
                format!("{n} in {w}")
            } else {
                continue;
            };
            let k = wheres.len();
            let opt = f["type"].as_str().unwrap_or("").ends_with('?');
            let cond = if opt {
                format!("{n} == none or {}", cond)
            } else {
                cond
            };
            src.push_str(&format!(
                "    require {}, \"__where_{k}\"\n",
                super::params::rewrite_params(&expand_lets(&cond, &lets), &params.list)
                    .map_err(|e| (500, e))?
            ));
            wheres.push((n.to_string(), w.to_string()));
        }
        let message = cfg["message"].as_str().filter(|m| !m.trim().is_empty());
        if let Some(m) = message {
            let m = super::params::rewrite_params(&expand_lets(m, &lets), &params.list)
                .map_err(|e| (500, e))?;
            src.push_str(&format!("    let __message = {m}\n"));
        }
        for line in code.lines() {
            let l = super::params::rewrite_params(&expand_lets(line, &lets), &params.list)
                .map_err(|e| (500, e))?;
            src.push_str(&format!("    {l}\n"));
        }
        if code.trim().is_empty() {
            src.push_str("    return none\n");
        }
        let (over, errors) = self.form_check(&src)?;
        let Some(over) = over.filter(|_| errors.is_empty()) else {
            return Ok(reply(
                422,
                json!({ "error": format!("the form does not check: {}", errors.join("; ")), "diagnostics": errors }),
            ));
        };
        let omodel = over.model.as_ref().unwrap();
        let f = omodel
            .function("__form")
            .ok_or((500, "the form does not check".to_string()))?
            .clone();
        // the values of the fields, as their types read them
        let hash = auth::sha256_hex(&json!({ "app": app_id, "form": comp_id, "values": body["values"], "params": body["params"] }).to_string());
        if let Some((h, resp, tx)) =
            auth::idempotent(&mut self.conn, who.id, &key).map_err(|e| (500, e))?
        {
            if h != hash {
                return Ok(reply(
                    409,
                    json!({ "error": "this idempotency key was already used for another submit" }),
                ));
            }
            if let Some(s) = &self.sequencer {
                if s.durable().map_or(true, |d| d < tx) {
                    return Ok(reply(
                        503,
                        json!({ "error": "the first submit with this key is not kept yet: send it again in a moment" }),
                    ));
                }
            }
            let mut r: J = serde_json::from_str(&resp).unwrap_or(json!({}));
            r["tx"] = json!(tx);
            r["replayed"] = json!(true);
            return Ok(reply(200, r));
        }
        let mut i = self.interp(omodel, Some(p), None)?;
        let mut ctx = Ctx::default();
        ctx.scopes.push(Default::default());
        for prm in &params.list {
            ctx.bind(&prm.var, prm.value.clone());
        }
        let mut field_errors = serde_json::Map::new();
        for (k, n) in names.iter().enumerate() {
            let ty = f
                .params
                .iter()
                .find(|x| &x.name == n)
                .and_then(|x| x.ty.clone());
            let raw = body["values"].get(n).cloned().unwrap_or(J::Null);
            let optional = fields[k]["type"].as_str().unwrap_or("").ends_with('?');
            let v = if raw.is_null() || raw == json!("") {
                if !optional {
                    field_errors.insert(n.clone(), json!("required"));
                }
                V::None
            } else {
                match &ty {
                    Some(t) => match super::params::decode_value(&mut i, omodel, t, &raw) {
                        Ok(v) => v,
                        Err(e) => {
                            field_errors.insert(n.clone(), json!(e));
                            V::None
                        }
                    },
                    None => quadra_api::json::decode(&mut i, omodel, None, &raw, &|_| None)
                        .unwrap_or(V::None),
                }
            };
            ctx.bind(n, v);
        }
        if !field_errors.is_empty() {
            let first = field_errors.keys().next().cloned().unwrap_or_default();
            return Ok(reply(
                422,
                json!({ "error": format!("{first}: {}", field_errors[&first].as_str().unwrap_or("")), "field": first, "fields": field_errors }),
            ));
        }
        let conn = Pooled::take(&self.pool, &self.url).map_err(|e| (500, e))?;
        let db = Database::with(conn, self.schema.clone());
        i.materialize = materialized_formulas(&db.schema);
        i.sink = Some(self.sink(db, omodel));
        let body_stmts = f.body.clone().unwrap_or_default();
        let result: RefCell<(V, Option<String>)> = RefCell::new((V::None, None));
        let user = who.id;
        let label = comp["title"].as_str().unwrap_or(comp_id).to_string();
        let r = i.with_cause(format!("form {label} of app {}", app.name), |i| {
            i.transaction(|i| {
                // the last statement, an expression, is the form's result
                let (init, last) = match body_stmts.split_last() {
                    Some((l, init)) if matches!(l.kind, StmtKind::Expr(_)) => (init, Some(l)),
                    _ => (&body_stmts[..], None),
                };
                match i.stmts(init, &mut ctx) {
                    Ok(()) | Err(Flow::Return(_)) => {}
                    Err(e) => return Err(e),
                }
                let v = match last.map(|l| &l.kind) {
                    Some(StmtKind::Expr(e)) => i.eval(e, &mut ctx)?,
                    _ => V::None,
                };
                let msg = match ctx.local("__message").cloned() {
                    Some(V::Text(t)) => Some(t.to_string()),
                    Some(other) if !matches!(other, V::None) => Some(i.display(&other)),
                    _ => None,
                };
                let result_json = record_json(i, &v);
                let response = json!({ "result": result_json, "message": msg }).to_string();
                i.store.idempotency = Some((user, key.clone(), hash.clone(), response));
                *result.borrow_mut() = (v, msg);
                Ok(())
            })
        });
        let (v, msg) = result.into_inner();
        match r {
            Ok(()) => {
                let res = record_json(&mut i, &v);
                Ok(reply(
                    200,
                    json!({ "tx": i.store.tx, "result": res, "message": msg }),
                ))
            }
            Err(Flow::Conflict(c)) => {
                let list: Vec<J> = c.iter().map(|x| json!({ "tx": x.tx, "at": x.at, "user": x.user, "cause": x.cause, "what": x.what })).collect();
                Ok(reply(
                    409,
                    json!({ "error": "changed by others meanwhile: submit again", "conflicts": list }),
                ))
            }
            Err(Flow::Fail(m)) => {
                // a `where` of a field: at the field
                if let Some(k) = m
                    .strip_prefix("__where_")
                    .and_then(|k| k.trim().parse::<usize>().ok())
                {
                    let (n, w) = wheres.get(k).cloned().unwrap_or_default();
                    return Ok(reply(
                        422,
                        json!({ "error": format!("{n}: must be {w}"), "field": n }),
                    ));
                }
                let field = names
                    .iter()
                    .find(|n| m.contains(&format!("`{n}`")) || m.starts_with(&format!("{n}:")))
                    .cloned();
                let status =
                    if m.starts_with("permission denied") || m.contains(": permission denied") {
                        403
                    } else {
                        422
                    };
                Ok(reply(status, json!({ "error": m, "field": field })))
            }
            Err(other) => Ok(reply(422, json!({ "error": describe(other) }))),
        }
    }

    /// The form's function checked with the project (cached by its source, as workbooks are).
    fn form_check(
        &mut self,
        src: &str,
    ) -> Result<(Option<Rc<quadra_analysis::Checked>>, Vec<String>), Fail> {
        let key = format!("form\u{1}{src}");
        if let Some(c) = self.form_checks.get(&key) {
            return Ok(c.clone());
        }
        let path = self.root.join("tests/__form__.qd");
        let mut sources = self.files.clone();
        sources.push((path.clone(), src.to_string()));
        let project = Project::from_sources(&self.root, sources);
        let c = Rc::new(quadra_analysis::check(&project));
        let file = project.file_by_path(&path).map(|f| f.id);
        let errors: Vec<String> = c
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| {
                if Some(d.file) == file {
                    d.message.replace("__p_", "@")
                } else {
                    format!("the code: {}", d.message)
                }
            })
            .collect();
        let out = (c.model.is_some().then_some(c), errors);
        if self.form_checks.len() > 32 {
            self.form_checks.clear();
        }
        self.form_checks.insert(key, out.clone());
        Ok(out)
    }
}

/// A record (or a value) as the answer of a submit shows it.
fn record_json(i: &mut Interp, v: &V) -> J {
    match v {
        V::None => J::Null,
        V::Ref { ent, id, .. } => {
            json!({ "entity": ent.to_string(), "id": id, "display": i.display(v) })
        }
        other => json!({ "value": i.display(other) }),
    }
}

/// The definition of a row as a file keeps it (shares with roles and everyone only).
pub fn definition(v: &ViewRow, shares: &[Share]) -> J {
    let sh: Vec<J> = shares
        .iter()
        .filter_map(|s| match s.target.split_once(':') {
            Some(("role", r)) => Some(json!({ "type": "role", "name": r, "edit": s.edit })),
            None if s.target == "everyone" => Some(json!({ "type": "everyone", "edit": s.edit })),
            _ => None,
        })
        .collect();
    json!({ "kind": v.kind, "entity": v.entity, "type": v.vtype, "name": v.name, "description": v.description, "config": v.config, "shares": sh })
}

/// A file name for an app or a view: lower-case words joined by `-`.
pub fn file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "app".into()
    } else {
        out
    }
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("qdi.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The `let`s of an app's configuration, in order, each with the ones before expanded.
pub fn lets_of(cfg: &J) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for l in cfg["lets"].as_array().into_iter().flatten() {
        if let (Some(n), Some(e)) = (l["name"].as_str(), l["expr"].as_str()) {
            let e = expand_lets(e, &out);
            out.push((n.to_string(), e));
        }
    }
    out
}

/// An expression with the app's `let`s in it: a name of a `let` (not a field after `.`, not a
/// parameter name before `=`, not in a string) stands for its expression in parentheses.
pub fn expand_lets(src: &str, lets: &[(String, String)]) -> String {
    if lets.is_empty() {
        return src.to_string();
    }
    use quadra_syntax::SyntaxKind as K;
    let (toks, _) = quadra_syntax::lexer::tokenize(src);
    let sig: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            !matches!(
                t.kind,
                K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT
            )
        })
        .map(|(k, _)| k)
        .collect();
    let mut out = String::with_capacity(src.len() + 32);
    let mut last = 0usize;
    for (n, &k) in sig.iter().enumerate() {
        let t = &toks[k];
        if t.kind != K::IDENT {
            continue;
        }
        let name = &src[t.range];
        let Some((_, e)) = lets.iter().find(|(l, _)| l == name) else {
            continue;
        };
        let prev = n.checked_sub(1).map(|p| toks[sig[p]].kind);
        let next = sig.get(n + 1).map(|&x| toks[x].kind);
        if matches!(prev, Some(K::DOT | K::QUESTION_DOT | K::AT))
            || matches!(next, Some(K::EQ | K::FAT_ARROW))
        {
            continue;
        }
        let (a, b) = (usize::from(t.range.start()), usize::from(t.range.end()));
        out.push_str(&src[last..a]);
        out.push('(');
        out.push_str(e);
        out.push(')');
        last = b;
    }
    out.push_str(&src[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lets() {
        let l = lets_of(
            &json!({ "lets": [{ "name": "sold", "expr": "Order.where(o => o.state is Confirmed)" }, { "name": "n", "expr": "sold.count()" }] }),
        );
        assert_eq!(l[1].1, "(Order.where(o => o.state is Confirmed)).count()");
        assert_eq!(
            expand_lets("n + o.n + f(n=1) + \"n\"", &l),
            "((Order.where(o => o.state is Confirmed)).count()) + o.n + f(n=1) + \"n\""
        );
    }

    #[test]
    fn names() {
        assert_eq!(file_name("Sales cockpit"), "sales-cockpit");
        assert_eq!(file_name("Région × état!"), "région-état");
    }
}
