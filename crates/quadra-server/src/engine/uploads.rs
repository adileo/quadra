//! The files of `File` fields over the API.
//!
//! - `POST /v1/uploads`: who may write the field asks for an upload; it gets a signed URL to
//!   put the bytes to (15 minutes), straight into the store of files.
//! - `POST /v1/tx` with `upload:<id>` as a field's value: before the transaction, the server
//!   reads the bytes (`check`, off the engine), recognizes their type, checks them against
//!   the field, and keeps them under their hash; the transaction then names `sha256:<hex>`.
//! - `GET /v1/records/{E}/{id}/files/{field}[/{n}]`: who may read the field is sent to a
//!   signed URL of the file (5 minutes).
//!
//! Uploads go after a day, used or not.

use super::*;
use quadra_blobs::{detect, Files};
use quadra_ir::files::{accepts, show_size, FileSpec};
use quadra_store::files as kept;
use std::sync::Arc;

/// The largest file of a field without `max`.
pub(crate) const DEFAULT_MAX: u64 = 100 << 20;

/// How long an upload waits for the transaction that names it.
const UPLOAD_KEPT_HOURS: i64 = 24;

fn stamp(t: chrono::NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `a` or `an`, before a type: `an application/pdf`, `a text/plain`.
fn article(t: &str) -> &'static str {
    if t.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

/// Whether a declared type is `File`, `File?`, `[File]` or `{File}`.
fn is_file(t: &quadra_ir::Ty) -> bool {
    use quadra_ir::Ty;
    match t {
        Ty::Optional(i) | Ty::List(i) | Ty::Set(i) => is_file(i),
        Ty::Path { segments, .. } => segments.len() == 1 && segments[0] == "File",
        _ => false,
    }
}

/// The `File` field `entity.field`, and what it takes.
fn file_field<'m>(
    model: &'m Model,
    entity: &str,
    field: &str,
) -> Result<(&'m quadra_ir::Entity, &'m quadra_ir::Field, &'m FileSpec), Fail> {
    let e = model
        .entity(entity)
        .filter(|e| e.kind == EntityKind::Entity && !e.temporary && !e.is_ledger)
        .ok_or((404, format!("unknown entity `{entity}`")))?;
    let f = model
        .find_field(e, field)
        .ok_or((404, format!("`{entity}` has no field `{field}`")))?;
    match (&f.file, f.ty.as_ref().is_some_and(is_file)) {
        (Some(spec), true) if f.formula.is_none() => Ok((e, f, spec)),
        _ => Err((400, format!("`{entity}.{field}` is not a `File` field"))),
    }
}

/// What the field takes, said: `pdf, image`, or `any file`.
fn takes(spec: &[String]) -> String {
    if spec.is_empty() {
        "any file".into()
    } else {
        spec.join(", ")
    }
}

/// An upload to check before a transaction: what the server needs to check its bytes off the
/// engine (`check`).
fn pending(u: &kept::Upload, spec: &FileSpec) -> J {
    json!({
        "id": u.id,
        "name": u.name,
        "size": u.size,
        "field": format!("{}.{}", u.entity, u.field),
        "accept": spec.accept.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>(),
        "max": spec.max_bytes().unwrap_or(DEFAULT_MAX),
    })
}

/// Reads the bytes of an upload from the store and checks them against what its field takes:
/// their type recognized from them, their size; then keeps them under their hash and removes
/// the upload. What the file is, or why it is refused (the upload removed too).
pub(crate) fn check(files: &Files, p: &J) -> Result<kept::File, String> {
    let id = p["id"].as_str().unwrap_or("");
    let name = p["name"].as_str().unwrap_or("file");
    let field = p["field"].as_str().unwrap_or("");
    let declared = p["size"].as_u64().unwrap_or(0);
    let max = p["max"].as_u64().unwrap_or(DEFAULT_MAX);
    let accept: Vec<String> = p["accept"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let key = quadra_blobs::upload_key(id);
    let bytes = files.blobs.get(&key)?.ok_or(format!(
        "{name}: the file was not uploaded (upload:{id}): put its bytes to the upload's URL first"
    ))?;
    let refuse = |why: String| {
        let _ = files.blobs.delete(&key);
        Err(why)
    };
    let size = bytes.len() as u64;
    if size > max {
        return refuse(format!(
            "{name}: the file is {}, but {field} takes files up to {}",
            show_size(size),
            show_size(max)
        ));
    }
    if size != declared {
        return refuse(format!(
            "{name}: the file uploaded is {size} bytes, not the {declared} its upload said"
        ));
    }
    let mime = detect::detect(&bytes, name);
    if !accepts(accept.iter().map(String::as_str), mime) {
        return refuse(format!(
            "{name}: the file is {} {mime}, but {field} accepts {}",
            article(mime),
            takes(&accept)
        ));
    }
    let hash = quadra_blobs::sha256_hex(&bytes);
    files.keep(&hash, &bytes, mime, name)?;
    files.blobs.delete(&key)?;
    Ok(kept::File {
        hash,
        name: name.to_string(),
        ctype: mime.to_string(),
        size,
        created_at: stamp(now()),
    })
}

impl Engine {
    /// The store of the project's files, as its `quadra.toml` says.
    pub(crate) fn files(&self) -> Result<Arc<Files>, String> {
        let model = self
            .checked
            .model
            .as_ref()
            .ok_or("the project has errors: its files are not reachable")?;
        quadra_blobs::open(&self.root, &model.manifest, &self.env)
    }

    /// `POST /v1/uploads {entity, field, name, size}`: for who may write the field, a file the
    /// field may take by what is declared of it; a signed URL to put its bytes to.
    pub(super) fn upload(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        if who.read_only {
            return Err((403, "this token is read-only".into()));
        }
        let checked = self.checked.clone();
        let model = checked
            .model
            .as_ref()
            .ok_or((503, "the project has errors".to_string()))?;
        let text = |k: &str| body[k].as_str().unwrap_or("").to_string();
        let (entity, field, name) = (text("entity"), text("field"), text("name"));
        let size = body["size"]
            .as_u64()
            .ok_or((400, "`size` is the size of the file, in bytes".to_string()))?;
        if name.trim().is_empty() || name.len() > 255 || name.contains(['/', '\\', '\0']) {
            return Err((
                400,
                "`name` is the file's name, without folders (at most 255 characters)".into(),
            ));
        }
        let (e, _, spec) = file_field(model, &entity, &field)?;
        let mut i = self.interp(model, Some(who), None)?;
        let may = !i.hidden(&e.name, &field)
            && (i.can("edit", &e.name, Some(&field), None) || i.can("create", &e.name, None, None));
        drop(i);
        if !may {
            return Err((
                403,
                format!("permission denied: cannot write {entity}.{field}"),
            ));
        }
        let label = format!("{entity}.{field}");
        let accept: Vec<String> = spec.accept.iter().map(|(a, _)| a.clone()).collect();
        if let Some(t) = detect::guess(&name).filter(|t| !spec.accepts(t)) {
            return Err((
                422,
                format!(
                    "{name}: the file is {} {t}, but {label} accepts {}",
                    article(t),
                    takes(&accept)
                ),
            ));
        }
        let max = spec.max_bytes().unwrap_or(DEFAULT_MAX);
        if size == 0 {
            return Err((422, format!("{name}: the file is empty")));
        }
        if size > max {
            return Err((
                422,
                format!(
                    "{name}: the file is {}, but {label} takes files up to {}",
                    show_size(size),
                    show_size(max)
                ),
            ));
        }
        let files = self.files().map_err(|e| (503, e))?;
        let id = agent_secret(16);
        let at = now();
        let expires = at + chrono::Duration::seconds(quadra_blobs::UPLOAD_FOR.as_secs() as i64);
        let u = kept::Upload {
            id: id.clone(),
            entity,
            field,
            user_id: who.user_id,
            name,
            size,
            created_at: stamp(at),
            expires_at: stamp(expires),
            used: false,
            hash: None,
        };
        self.side(move |c| kept::add_upload(c, &u))
            .map_err(|e| (500, e))?;
        let signed = files
            .blobs
            .put_url(
                &quadra_blobs::upload_key(&id),
                size,
                quadra_blobs::UPLOAD_FOR,
            )
            .map_err(|e| (503, e))?;
        let headers: serde_json::Map<String, J> = signed
            .headers
            .into_iter()
            .map(|(k, v)| (k, J::String(v)))
            .collect();
        Ok(reply(
            200,
            json!({
                "id": id,
                "value": format!("upload:{id}"),
                "url": signed.url,
                "method": signed.method,
                "headers": headers,
                "expires_at": format!("{}Z", stamp(expires)),
            }),
        ))
    }

    /// The upload `upload:<id>` names, for this user, entity and field, not used and not too
    /// old.
    fn upload_for(
        &mut self,
        who: &Principal,
        id: &str,
        entity: &str,
        field: &str,
    ) -> Result<kept::Upload, Fail> {
        let u = kept::upload(&mut self.conn, id)
            .map_err(|e| (500, e))?
            .ok_or((422, format!("upload:{id}: there is no such upload")))?;
        if u.user_id != who.user_id {
            return Err((422, format!("upload:{id}: the upload is someone else's")));
        }
        if u.entity != entity || u.field != field {
            return Err((
                422,
                format!(
                    "upload:{id}: the upload is for {}.{}, not {entity}.{field}",
                    u.entity, u.field
                ),
            ));
        }
        if u.used {
            return Err((
                422,
                format!("upload:{id}: the upload was already used: upload the file again"),
            ));
        }
        let oldest = stamp(now() - chrono::Duration::hours(UPLOAD_KEPT_HOURS));
        if u.created_at < oldest {
            return Err((
                422,
                format!("upload:{id}: the upload has expired: upload the file again"),
            ));
        }
        Ok(u)
    }

    /// The values a transaction writes to `File` fields (`create` and `update`), each with
    /// what the field takes: `f(op, entity, field, value, spec)`.
    fn file_values(
        model: &Model,
        ops: &[J],
        mut f: impl FnMut(usize, &str, &str, &str, &FileSpec) -> Result<Option<String>, Fail>,
    ) -> Result<Vec<J>, Fail> {
        let mut out = ops.to_vec();
        for (n, op) in out.iter_mut().enumerate() {
            if !matches!(op["op"].as_str(), Some("create" | "update")) {
                continue;
            }
            let ent = op["entity"].as_str().unwrap_or("").to_string();
            let Some(e) = model.entity(&ent) else {
                continue;
            };
            let Some(values) = op.get_mut("values").and_then(|v| v.as_object_mut()) else {
                continue;
            };
            for (k, v) in values.iter_mut() {
                let Some(fd) = model.find_field(e, k) else {
                    continue;
                };
                let (Some(spec), true) = (&fd.file, fd.ty.as_ref().is_some_and(is_file)) else {
                    continue;
                };
                let items: Vec<&mut J> = match v {
                    J::String(_) => vec![v],
                    J::Array(a) => a.iter_mut().collect(),
                    _ => continue,
                };
                for it in items {
                    if let Some(s) = it.as_str().map(str::to_string) {
                        if let Some(new) = f(n, &ent, k, &s, spec)? {
                            *it = J::String(new);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// `POST /v1/tx` naming uploads, before it is sent: the uploads whose bytes are still to
    /// be checked (`check`), or why one of them cannot be named. A transaction already done
    /// with this key has none: it is answered again as it was.
    pub(super) fn uploads_due(
        &mut self,
        who: &Principal,
        body: &J,
        key: &str,
    ) -> Result<Reply, Fail> {
        if auth::idempotent(&mut self.conn, who.user_id, key)
            .map_err(|e| (500, e))?
            .is_some()
        {
            return Ok(reply(200, json!({ "uploads": [] })));
        }
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return Ok(reply(200, json!({ "uploads": [] })));
        };
        let ops = body["ops"].as_array().cloned().unwrap_or_default();
        let mut due = Vec::new();
        Self::file_values(model, &ops, |_, ent, field, value, spec| {
            if let Some(id) = kept::upload_of(value) {
                let u = self.upload_for(who, id, ent, field)?;
                if u.hash.is_none() && !due.iter().any(|d: &J| d["id"] == json!(u.id)) {
                    due.push(pending(&u, spec));
                }
            }
            Ok(None)
        })?;
        Ok(reply(200, json!({ "uploads": due })))
    }

    /// The uploads checked (`check`): their files kept (what they are) and the uploads marked
    /// with their hash, ready for the transaction.
    pub(super) fn uploads_checked(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        for f in body["files"].as_array().cloned().unwrap_or_default() {
            let id = f["id"].as_str().unwrap_or("").to_string();
            let u = kept::upload(&mut self.conn, &id)
                .map_err(|e| (500, e))?
                .filter(|u| u.user_id == who.user_id && !u.used)
                .ok_or((422, format!("upload:{id}: there is no such upload")))?;
            let file = kept::File {
                hash: f["hash"].as_str().unwrap_or("").to_string(),
                name: f["name"].as_str().unwrap_or(&u.name).to_string(),
                ctype: f["type"].as_str().unwrap_or("").to_string(),
                size: f["size"].as_u64().unwrap_or(0),
                created_at: stamp(now()),
            };
            self.side(move |c| {
                kept::put(c, &file)?;
                kept::upload_checked(c, &id, &file.hash)
            })
            .map_err(|e| (500, e))?;
        }
        Ok(reply(200, json!({})))
    }

    /// The operations of a transaction with each `upload:<id>` of a `File` field replaced by
    /// the `sha256:<hex>` of its file, and the uploads named. A `File` field takes only an
    /// upload, or a file it already has: never another value, so that no one names a file
    /// they have not uploaded.
    pub(super) fn resolve_uploads(
        &mut self,
        who: &Principal,
        model: &Model,
        ops: &[J],
    ) -> Result<(Vec<J>, Vec<String>), Fail> {
        let mut used = Vec::new();
        let mut current: HashMap<(u64, String), Vec<String>> = HashMap::new();
        let out = Self::file_values(model, ops, |n, ent, field, value, _| {
            if let Some(id) = kept::upload_of(value) {
                let u = self.upload_for(who, id, ent, field)?;
                let hash = u.hash.ok_or((
                    422,
                    format!(
                        "upload:{id}: its file was not checked yet: send the transaction again"
                    ),
                ))?;
                used.push(u.id);
                return Ok(Some(kept::reference(&hash)));
            }
            let has = match (ops[n]["op"].as_str(), ops[n]["id"].as_u64()) {
                (Some("update"), Some(rec)) => {
                    let key = (rec, field.to_string());
                    if !current.contains_key(&key) {
                        let mut i = self.interp(model, None, None)?;
                        let now: Vec<String> = match i.store.read(rec, field, None) {
                            Some(V::Text(t)) => vec![t.to_string()],
                            Some(V::List(l) | V::Set(l)) => l
                                .iter()
                                .filter_map(|x| match x {
                                    V::Text(t) => Some(t.to_string()),
                                    _ => None,
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        current.insert(key.clone(), now);
                    }
                    current[&key].iter().any(|x| x == value)
                }
                _ => false,
            };
            if has {
                Ok(None)
            } else {
                Err((
                    422,
                    format!(
                        "operation {}: {ent}.{field} takes a file uploaded (`upload:<id>` from POST /v1/uploads) or one it already has, not `{}`",
                        n + 1,
                        value.chars().take(80).collect::<String>()
                    ),
                ))
            }
        })?;
        Ok((out, used))
    }

    /// `GET /v1/records/{E}/{id}/files/{field}[/{n}]`, for one who may read that field: where
    /// the file is, a signed URL (`{url, name, type, size, hash}`).
    pub(super) fn file(
        &mut self,
        who: &Principal,
        entity: &str,
        id: u64,
        field: &str,
        n: Option<usize>,
        download: bool,
    ) -> Result<Reply, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        let actual = i
            .store
            .get(id)
            .map(|r| r.ent.to_string())
            .filter(|e| e == entity)
            .ok_or((404, format!("{entity}#{id} not found")))?;
        let rec = V::Ref {
            ent: Rc::from(actual.as_str()),
            id,
        };
        if !i.can("read", &actual, Some(field), Some(&rec)) || i.hidden(&actual, field) {
            return Err((
                403,
                format!("permission denied: cannot read {actual}.{field}"),
            ));
        }
        let value = match (i.store.read(id, field, None), n) {
            (Some(V::Text(t)), None | Some(0)) => t.to_string(),
            (Some(V::List(l) | V::Set(l)), Some(k)) => match l.get(k) {
                Some(V::Text(t)) => t.to_string(),
                _ => {
                    return Err((
                        404,
                        format!(
                            "{actual}#{id} has no file {k} in `{field}` (it has {})",
                            l.len()
                        ),
                    ))
                }
            },
            (Some(V::List(_) | V::Set(_)), None) => {
                return Err((
                    400,
                    format!(
                    "`{actual}.{field}` is a list: name the file by its place, /files/{field}/0"
                ),
                ))
            }
            _ => return Err((404, format!("{actual}#{id} has no file in `{field}` yet"))),
        };
        drop(i);
        let Some(hash) = kept::hash_of(&value) else {
            return Err((
                404,
                format!("`{actual}.{field}` names no file kept by the app"),
            ));
        };
        let f = kept::get(&mut self.conn, hash)
            .map_err(|e| (500, e))?
            .ok_or((404, format!("the file {hash} is not kept here")))?;
        let url = self
            .files()
            .and_then(|s| s.download_url(&f.hash, &f.ctype, &f.name, download))
            .map_err(|e| (503, e))?;
        Ok(reply(
            200,
            json!({"url": url, "name": f.name, "type": f.ctype, "size": f.size, "hash": f.hash}),
        ))
    }

    /// Once an hour: the uploads of more than a day removed, with their bytes if they are
    /// still there (never named by a transaction).
    pub(super) fn prune_uploads(&mut self) {
        if self
            .uploads_pruned_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3600))
        {
            return;
        }
        self.uploads_pruned_at = Some(std::time::Instant::now());
        self.prune_files();
        let before = stamp(now() - chrono::Duration::hours(UPLOAD_KEPT_HOURS));
        let old = match kept::old_uploads(&mut self.conn, &before, 1000) {
            Ok(o) if !o.is_empty() => o,
            Ok(_) => return,
            Err(e) => {
                eprintln!("uploads: the old ones were not read: {e}");
                return;
            }
        };
        let files = match self.files() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("uploads: {e}");
                return;
            }
        };
        let mut gone = Vec::new();
        for u in old {
            match files.blobs.delete(&quadra_blobs::upload_key(&u.id)) {
                Ok(()) => gone.push(u.id),
                Err(e) => eprintln!("uploads: upload:{} not removed: {e}", u.id),
            }
        }
        if let Err(e) = self.side(move |c| kept::remove_uploads(c, &gone)) {
            eprintln!("uploads: the old ones were not removed: {e}");
        }
    }

    /// The files no value names any more (their values erased, or replaced and taken out of
    /// the history): removed from the store and from `_quadra_files`. A file kept less than an
    /// hour ago is left alone (its record may not be committed yet).
    pub(super) fn prune_files(&mut self) {
        let before = stamp(now() - chrono::Duration::hours(1));
        let rows = match self.conn.query(
            "SELECT \"hash\" FROM \"_quadra_files\" WHERE \"created_at\" < ?text",
            &[quadra_store::conn::Sql::Text(before)],
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("files: not read: {e}");
                return;
            }
        };
        let mut unused = Vec::new();
        for r in rows {
            let Some(h) = r[0].clone() else { continue };
            let named = self.conn.query(
                "SELECT 1 FROM \"_quadra_history\" WHERE \"value\" LIKE ?text LIMIT 1",
                &[quadra_store::conn::Sql::Text(format!("%sha256:{h}%"))],
            );
            if matches!(named, Ok(ref v) if v.is_empty()) {
                unused.push(h);
            }
        }
        if unused.is_empty() {
            return;
        }
        let files = match self.files() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("files: {e}");
                return;
            }
        };
        let mut gone = Vec::new();
        for h in unused {
            match files.blobs.delete(&quadra_blobs::file_key(&h)) {
                Ok(()) => gone.push(h),
                Err(e) => eprintln!("files: sha256:{h} not removed: {e}"),
            }
        }
        if let Err(e) = self.side(move |c| {
            for h in &gone {
                c.exec(
                    "DELETE FROM \"_quadra_files\" WHERE \"hash\" = ?text",
                    &[quadra_store::conn::Sql::Text(h.clone())],
                )?;
            }
            Ok(())
        }) {
            eprintln!("files: not removed from the table: {e}");
        }
    }
}
