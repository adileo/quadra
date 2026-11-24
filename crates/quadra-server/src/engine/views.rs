//! Saved views, boards and teams: what users create in the web client and share
//! (`/v1/views`, `/v1/teams`, `/v1/directory`; the HTTP API reference (quadra.run/reference/http-api), "Views and teams").
//!
//! A view is data, not code: its configuration (columns, filter, order, groups, board) is
//! stored in system tables (`quadra_store::views`), never in the model. Sharing shares the
//! configuration, never data: a view always runs as the user who opens it, with their row
//! rules and hidden fields, so two people may see different rows in the same view.
//!
//! - **Who sees a view**: its owner; the users, teams (their members) and roles (the users
//!   who have the role, directly or through a role that extends it) it is shared with;
//!   everyone when shared with everyone. Admins — the users who may edit `User.roles` — may
//!   change and share every view and manage every team.
//! - **Who changes it**: its owner, admins, and those it is shared with `edit` (co-owners:
//!   they change it, they do not share or delete it). Others with access use it and save it as
//!   their own copy.
//! - **Expressions** (`where`, personal columns, color rules) are checked when a view is saved,
//!   as `POST /v1/query` checks them; field paths against the model.
//! - **Deploys**: after a new model is served, each view checked against another model is
//!   checked again: the field renames of the migrations since it was last checked are applied
//!   to its paths and expressions (recorded in its history, `deploy: renamed …`); what no longer
//!   fits (a removed field, a type that breaks an expression) marks it broken with the
//!   checker's messages. A view is never deleted by a deploy.
//! - **Folders** (`kind = "folder"`) hold apps and folders, nested at most `MAX_DEPTH` deep
//!   (`parent`: the folder an app or a folder is in; saved views of entities are in none). A
//!   folder's name is a valid directory name, unique among the folders next to it (whatever
//!   their owner: the tree is one for everyone, as directories on a disk). Folders are shared
//!   as apps are, and sharing a folder shares the folder only, never what is in it: each app
//!   keeps its own shares. A user is shown the folders they own or that are shared with them,
//!   and those holding (at any depth) an app or a folder they are shown (`implied`: shown for
//!   their content, used only to find it). Putting something in a folder takes the right to
//!   change it (owner, co-owners, admins) and the right to change the folder; deleting a
//!   folder moves what it holds to the folder it was in.

use super::*;
use quadra_store::views::{self as store, Pref, Share, Stale, TeamRow, ViewChange, ViewRow};
use quadra_types::T;
use std::collections::HashSet;

/// The kinds of objects stored with views: saved views, and apps (engine/apps.rs) with the
/// same sharing and the same API.
const KINDS: &[&str] = &["view", "app", "folder"];
const TYPES: &[&str] = &["grid", "groups", "kanban"];
const MAX_CONFIG: usize = 64 * 1024;
/// How deep folders nest: a folder at the top is at depth 1.
const MAX_DEPTH: usize = 10;

/// Why a folder's name is not one (a valid directory name on every system: folders may become
/// directories of files one day).
fn folder_name_problem(name: &str) -> Option<String> {
    if name == "." || name == ".." || name.ends_with('.') {
        return Some(format!("`{name}`: a folder's name does not end with `.`"));
    }
    if let Some(c) = name.chars().find(|c| c.is_control() || "/\\:*?\"<>|".contains(*c)) {
        return Some(format!("a folder's name has no `{}` (nor / \\ : * ? \" < > |)", c.escape_default()));
    }
    None
}

/// The parent asked for in a request: `None` when `parent` is absent, `Some(None)`: the top.
fn parent_of(body: &J) -> Result<Option<Option<u64>>, Fail> {
    match body.get("parent") {
        None => Ok(None),
        Some(J::Null) => Ok(Some(None)),
        Some(p) => p.as_u64().map(|p| Some(Some(p))).ok_or_else(|| bad("`parent`: a folder's id, or null (the top)")),
    }
}

/// The folders from `id` up to the top (`id` first); stops on a loop.
fn ancestry(folders: &HashMap<u64, ViewRow>, id: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        if out.contains(&c) || !folders.contains_key(&c) {
            break;
        }
        out.push(c);
        cur = folders[&c].parent;
    }
    out
}

/// How many levels of folders a folder holds, itself included.
fn height(folders: &HashMap<u64, ViewRow>, id: u64) -> usize {
    folders.values().filter_map(|f| ancestry(folders, f.id).iter().position(|a| *a == id)).max().unwrap_or(0) + 1
}

/// The caller, as views see them.
pub(super) struct Who {
    pub id: u64,
    pub admin: bool,
    pub roles: Vec<String>,
    pub teams: HashSet<u64>,
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Access {
    Use,
    Edit,
    Admin,
    Owner,
}

impl Access {
    fn name(self) -> &'static str {
        match self {
            Access::Use => "use",
            Access::Edit => "edit",
            Access::Admin => "admin",
            Access::Owner => "owner",
        }
    }
}

/// What the caller may do with a view; `None`: they may not see it.
pub(super) fn access(who: &Who, v: &ViewRow, shares: &[Share]) -> Option<Access> {
    if v.owner == who.id {
        return Some(Access::Owner);
    }
    let mut best: Option<Access> = None;
    for s in shares {
        let applies = match s.target.split_once(':') {
            None => s.target == "everyone",
            Some(("user", id)) => id.parse::<u64>().ok() == Some(who.id),
            Some(("team", id)) => id.parse::<u64>().map_or(false, |t| who.teams.contains(&t)),
            Some(("role", r)) => who.roles.iter().any(|x| x == r),
            _ => false,
        };
        if applies {
            let a = if s.edit { Access::Edit } else { Access::Use };
            best = Some(best.map_or(a, |b| b.max(a)));
        }
    }
    if who.admin {
        best = Some(Access::Admin.max(best.unwrap_or(Access::Admin)));
    }
    best
}

/// Is `access` (without the admin's rights) given by the owner or a share: the views listed
/// to a user, admins included.
fn listed(who: &Who, v: &ViewRow, shares: &[Share]) -> bool {
    let plain = Who { id: who.id, admin: false, roles: who.roles.clone(), teams: who.teams.clone(), read_only: who.read_only };
    access(&plain, v, shares).is_some()
}

pub(super) fn at_now() -> String {
    now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub(super) fn bad(msg: impl Into<String>) -> Fail {
    (400, msg.into())
}

fn stale_reply(what: &str, s: Stale, names: &HashMap<u64, (String, String)>) -> Reply {
    let list: Vec<J> =
        s.0.iter()
            .map(|e| {
                json!({
                    "version": e.version, "at": e.at, "user": e.user.and_then(|u| names.get(&u)).map(|n| n.0.clone()),
                    "cause": e.action, "what": e.detail["changed"].clone(),
                })
            })
            .collect();
    reply(409, json!({ "error": format!("this {what} was changed by others since you read it: review their changes and try again"), "conflicts": list }))
}

/// A short single-line text from a JSON body.
fn text_of(body: &J, key: &str, max: usize, required: bool) -> Result<Option<String>, Fail> {
    match &body[key] {
        J::Null if !required => Ok(None),
        J::String(s) => {
            let s = s.trim().to_string();
            if s.chars().count() > max || (required && s.is_empty()) {
                return Err(bad(format!("`{key}`: {} to {max} characters", if required { 1 } else { 0 })));
            }
            Ok(Some(s))
        }
        _ => Err(bad(format!("`{key}` is required (text)"))),
    }
}

/// The expressions a view's configuration holds, checked like those of a query: (where it
/// is, the formula name of the check, the expression).
fn config_items(cfg: &J) -> Result<Vec<(String, String, String)>, Fail> {
    let mut items = Vec::new();
    if let Some(w) = cfg["where"].as_str().filter(|w| !w.trim().is_empty()) {
        items.push(("where".to_string(), "__where".to_string(), w.to_string()));
    }
    if let Some(cols) = cfg["columns"].as_object() {
        for (n, e) in cols {
            let ok = !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(bad(format!("column name `{n}`: letters, digits and _ only")));
            }
            let e = e.as_str().ok_or_else(|| bad(format!("column `{n}`: an expression as text")))?;
            items.push((format!("column {n}"), format!("__col_{n}"), e.to_string()));
        }
    }
    if let Some(rules) = cfg["colors"].as_array() {
        for (k, r) in rules.iter().enumerate() {
            let e = r["expr"].as_str().ok_or_else(|| bad(format!("color rule {}: an expression as text", k + 1)))?;
            items.push((format!("color rule {}", k + 1), format!("__fmt_{k}"), e.to_string()));
        }
    }
    for (label, _, e) in &items {
        if e.contains('\n') || e.contains('\r') || e.len() > 2000 || e.trim().is_empty() {
            return Err(bad(format!("{label}: an expression must be one line of at most 2000 characters")));
        }
    }
    Ok(items)
}

fn put_items(cfg: &mut J, items: &[(String, String, String)]) {
    for (_, name, e) in items {
        if name == "__where" {
            cfg["where"] = json!(e);
        } else if let Some(n) = name.strip_prefix("__col_") {
            cfg["columns"][n] = json!(e);
        } else if let Some(k) = name.strip_prefix("__fmt_").and_then(|k| k.parse::<usize>().ok()) {
            cfg["colors"][k]["expr"] = json!(e);
        }
    }
}

/// The field paths of a configuration: (where it is, a pointer to it, the path). Personal
/// columns are not paths.
fn config_paths(cfg: &J) -> Vec<(String, Vec<J>, String)> {
    let personal: HashSet<String> = cfg["columns"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    let mut out = Vec::new();
    let mut add = |label: &str, at: Vec<J>, p: Option<&str>| {
        if let Some(p) = p.filter(|p| !p.is_empty() && !personal.contains(*p)) {
            out.push((label.to_string(), at, p.to_string()));
        }
    };
    for (k, f) in cfg["fields"].as_array().into_iter().flatten().enumerate() {
        add("column", vec![json!("fields"), json!(k)], f.as_str());
    }
    for (k, o) in cfg["order"].as_array().into_iter().flatten().enumerate() {
        add("order", vec![json!("order"), json!(k), json!("field")], o["field"].as_str());
    }
    for (k, g) in cfg["group"]["keys"].as_array().into_iter().flatten().enumerate() {
        add("group", vec![json!("group"), json!("keys"), json!(k), json!("field")], g["field"].as_str());
    }
    for (k, d) in cfg["drill"].as_array().into_iter().flatten().enumerate() {
        add("group filter", vec![json!("drill"), json!(k), json!("field")], d["field"].as_str());
    }
    let kb = &cfg["kanban"];
    add("board columns", vec![json!("kanban"), json!("field")], kb["field"].as_str());
    add("card title", vec![json!("kanban"), json!("title")], kb["title"].as_str());
    add("board total", vec![json!("kanban"), json!("aggregate")], kb["aggregate"].as_str());
    for (k, f) in kb["cards"].as_array().into_iter().flatten().enumerate() {
        add("card field", vec![json!("kanban"), json!("cards"), json!(k)], f.as_str());
    }
    out
}

fn set_at(cfg: &mut J, at: &[J], value: J) {
    let mut cur = cfg;
    for k in at {
        cur = match k {
            J::String(s) => &mut cur[s.as_str()],
            J::Number(n) => &mut cur[n.as_u64().unwrap_or(0) as usize],
            _ => return,
        };
    }
    *cur = value;
}

/// The keys of maps keyed by field path (`widths`, the group's `aggregates`): renamed with
/// the paths.
fn keyed_paths(cfg: &J) -> Vec<(Vec<&'static str>, String)> {
    let mut out = Vec::new();
    for (map, label) in [(vec!["widths"], "width"), (vec!["group", "aggregates"], "group total")] {
        let mut cur = cfg;
        for k in &map {
            cur = &cur[*k];
        }
        if let Some(o) = cur.as_object() {
            for k in o.keys() {
                let _ = label;
                out.push((map.clone(), k.clone()));
            }
        }
    }
    out
}

/// The system fields every record has, readable in paths.
const SYSTEM_FIELDS: &[&str] = &["id", "created_at", "created_by", "updated_at", "updated_by", "version"];

/// The entity a type leads to, through `?`.
fn entity_of(t: &T) -> Option<String> {
    match t {
        T::Ent(n) => Some(n.clone()),
        T::Opt(i) => entity_of(i),
        _ => None,
    }
}

/// A field path against the model: `Ok` or why it does not resolve.
fn check_path(model: &Model, report: &quadra_types::Report, entity: &str, path: &str) -> Result<(), String> {
    let mut ent = entity.to_string();
    let segs: Vec<&str> = path.split('.').collect();
    if segs.len() > 5 {
        return Err(format!("`{path}`: at most 4 relations"));
    }
    for (k, seg) in segs.iter().enumerate() {
        let Some(e) = model.entity(&ent) else {
            return Err(format!("`{ent}` no longer exists"));
        };
        if SYSTEM_FIELDS.contains(seg) {
            if k + 1 == segs.len() {
                return Ok(());
            }
            if *seg == "created_by" || *seg == "updated_by" {
                ent = "User".into();
                continue;
            }
            return Err(format!("`{path}`: `{seg}` is not a record"));
        }
        if !model.all_fields(e).iter().any(|f| f.name == *seg) {
            return Err(format!("`{ent}` has no field `{seg}`"));
        }
        if k + 1 == segs.len() {
            return Ok(());
        }
        match report.field_types.get(&(ent.clone(), seg.to_string())).and_then(entity_of) {
            Some(next) => ent = next,
            None => return Err(format!("`{path}`: `{ent}.{seg}` is not a record")),
        }
    }
    Ok(())
}

/// A name after the renames given, in order (`a → b`, then `b → c`).
fn renamed_name(renames: &[(String, String, String)], entity: &str, name: &str) -> Option<String> {
    let mut cur = name.to_string();
    for (e, old, new) in renames {
        if e == entity && *old == cur {
            cur = new.clone();
        }
    }
    (cur != name).then_some(cur)
}

/// A path after the renames given; `None` when nothing changed.
fn renamed_path(report: &quadra_types::Report, entity: &str, path: &str, renames: &[(String, String, String)]) -> Option<String> {
    let mut ent = Some(entity.to_string());
    let mut out = Vec::new();
    let mut changed = false;
    for seg in path.split('.') {
        let mut s = seg.to_string();
        if let Some(e) = &ent {
            if let Some(n) = renamed_name(renames, e, seg) {
                s = n;
                changed = true;
            }
        }
        ent = ent.and_then(|e| report.field_types.get(&(e, s.clone())).and_then(entity_of));
        out.push(s);
    }
    changed.then(|| out.join("."))
}

/// A diagnostic of a view's expression: (item, range in the expression, message).
type ItemDiag = (usize, Option<(usize, usize)>, String);

impl Engine {
    /// The model's identity: views checked against another one are checked again.
    pub(super) fn model_key(&self) -> String {
        let mut s = String::new();
        for (p, t) in &self.files {
            s.push_str(&p.to_string_lossy());
            s.push('\u{1}');
            s.push_str(t);
            s.push('\u{2}');
        }
        auth::sha256_hex(&s)[..16].to_string()
    }

    pub(super) fn who(&mut self, p: &Principal) -> Result<Who, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(p), None)?;
        let roles = i.roles_of(p.user_id);
        let admin = model.entity("User").is_some() && i.can("edit", "User", Some("roles"), None);
        drop(i);
        let teams = store::teams_of(&mut self.conn, p.user_id).map_err(|e| (500, e))?.into_iter().collect();
        Ok(Who { id: p.user_id, admin, roles, teams, read_only: p.read_only })
    }

    /// The table of `User` and its columns `login`, `name` (when it has one).
    fn users_table(&self) -> Option<(String, bool)> {
        let t = self.schema.tables.iter().find(|t| matches!(&t.source, quadra_store::schema::Source::Entity(e) if e == "User"))?;
        let has_name = self.checked.model.as_ref().and_then(|m| m.entity("User")).map_or(false, |e| self.checked.model.as_ref().unwrap().all_fields(e).iter().any(|f| f.name == "name" && f.formula.is_none()));
        Some((t.name.clone(), has_name))
    }

    /// Login and name of users, by id.
    fn user_names(&mut self, ids: &[u64]) -> HashMap<u64, (String, String)> {
        let mut out = HashMap::new();
        let ids: Vec<u64> = ids.iter().copied().collect::<HashSet<_>>().into_iter().collect();
        let Some((table, has_name)) = self.users_table() else {
            return out;
        };
        for chunk in ids.chunks(500) {
            let sql =
                format!("SELECT CAST(\"id\" AS TEXT), \"login\", {} FROM \"{table}\" WHERE \"id\" IN ({})", if has_name { "\"name\"" } else { "NULL" }, chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
            if let Ok(rows) = self.conn.query(&sql, &[]) {
                for r in rows {
                    if let Some(id) = r[0].as_deref().and_then(|x| x.parse().ok()) {
                        out.insert(id, (r[1].clone().unwrap_or_default(), r[2].clone().unwrap_or_default()));
                    }
                }
            }
        }
        out
    }

    fn user_exists(&mut self, id: u64) -> bool {
        !self.user_names(&[id]).is_empty()
    }

    /// The expressions of a view, checked on its entity as the expressions of a query are:
    /// the diagnostics of each (none: they all check).
    fn check_items(&mut self, entity: &str, items: &[(String, String, String)]) -> Vec<ItemDiag> {
        if items.is_empty() {
            return Vec::new();
        }
        let header = format!("extend {entity}:\n");
        let mut src = header.clone();
        let mut starts = Vec::new();
        let mut lines = Vec::new();
        for (_, n, e) in items {
            let prefix = format!("    {n} = ");
            lines.push(src.len());
            starts.push(src.len() + prefix.len());
            src.push_str(&prefix);
            src.push_str(e);
            src.push('\n');
        }
        let path = self.root.join("tests/__views__.qd");
        let mut sources = self.files.clone();
        sources.push((path.clone(), src));
        let project = Project::from_sources(&self.root, sources);
        let checked = quadra_analysis::check(&project);
        let file = project.file_by_path(&path).map(|f| f.id);
        let mut out = Vec::new();
        for d in checked.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error) {
            let (from, to) = (usize::from(d.range.start()), usize::from(d.range.end()));
            let item = if Some(d.file) == file && from >= header.len() { lines.iter().rposition(|s| *s <= from) } else { None };
            match item {
                Some(k) => {
                    let s = starts[k];
                    let len = items[k].2.len();
                    let range = (from >= s && to <= s + len).then(|| (from - s, to - s));
                    out.push((k, range, d.message.clone()));
                }
                // elsewhere (the overlay broke something else): every expression is suspect
                None => out.push((usize::MAX, None, d.message.clone())),
            }
        }
        if !out.is_empty() {
            return out;
        }
        let Some(over) = checked.model.as_ref() else {
            return vec![(usize::MAX, None, "the expressions do not check".into())];
        };
        let pairs: Vec<(String, String)> = items.iter().map(|(_, n, e)| (n.clone(), e.clone())).collect();
        if !quadra_api::same_shape(self.checked.model.as_ref().unwrap(), over, entity, &pairs) {
            return vec![(usize::MAX, None, "an expression must be a single value".into())];
        }
        // a filter and a color rule say yes or no
        for (k, (_, n, _)) in items.iter().enumerate() {
            if n == "__where" || n.starts_with("__fmt_") {
                match checked.report.field_types.get(&(entity.to_string(), n.clone())) {
                    Some(T::Bool | T::Unknown) | None => {}
                    Some(T::Opt(t)) if matches!(**t, T::Bool) => {}
                    Some(t) => out.push((k, None, format!("must be yes or no (it is {t})"))),
                }
            }
        }
        out
    }

    /// What does not fit the model in a configuration: the checker's messages.
    fn config_problems(&mut self, entity: &str, cfg: &J) -> Result<Vec<String>, Fail> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        if model.entity(entity).map_or(true, |e| e.kind != EntityKind::Entity || e.temporary) {
            return Ok(vec![format!("the entity `{entity}` no longer exists")]);
        }
        let items = config_items(cfg)?;
        let mut out = Vec::new();
        for (label, _, path) in config_paths(cfg) {
            if let Err(m) = check_path(model, &checked.report, entity, &path) {
                out.push(format!("{label} `{path}`: {m}"));
            }
        }
        for (k, _, m) in self.check_items(entity, &items) {
            let label = items.get(k).map(|i| i.0.as_str()).unwrap_or("expression");
            out.push(format!("{label}: {m}"));
        }
        Ok(out)
    }

    /// A configuration to save: an object, not too large, whose paths and expressions check.
    fn valid_config(&mut self, entity: &str, cfg: &J) -> Result<(), Reply> {
        if !cfg.is_object() {
            return Err(error(400, "`config` must be an object"));
        }
        if cfg.to_string().len() > MAX_CONFIG {
            return Err(error(400, "`config` is too large (at most 64 KB)"));
        }
        match self.config_problems(entity, cfg) {
            Err((s, m)) => Err(error(s, m)),
            Ok(p) if p.is_empty() => Ok(()),
            Ok(p) => Err(reply(400, json!({ "error": "the view does not check", "diagnostics": p }))),
        }
    }

    /// Who a view is shared with, from the request: `[{type, id | name, edit}]`.
    fn parse_shares(&mut self, body: &J) -> Result<Vec<Share>, Fail> {
        let list = body.as_array().ok_or_else(|| bad("`shares`: a list of {type, id, edit}"))?;
        if list.len() > 200 {
            return Err(bad("at most 200 shares"));
        }
        let model = self.checked.model.clone();
        let roles: Vec<String> = self.checked.model.as_ref().map(|m| m.roles.iter().map(|r| r.name.clone()).collect()).unwrap_or_default();
        let _ = model;
        let teams: HashSet<u64> = store::teams(&mut self.conn).map_err(|e| (500, e))?.into_iter().map(|t| t.id).collect();
        let mut out = Vec::new();
        for s in list {
            let edit = s["edit"].as_bool().unwrap_or(false);
            let target = match s["type"].as_str() {
                Some("everyone") => "everyone".to_string(),
                Some("user") => {
                    let id = s["id"].as_u64().ok_or_else(|| bad("share with a user: `id`"))?;
                    if !self.user_exists(id) {
                        return Err(bad(format!("no user #{id}")));
                    }
                    format!("user:{id}")
                }
                Some("team") => {
                    let id = s["id"].as_u64().ok_or_else(|| bad("share with a team: `id`"))?;
                    if !teams.contains(&id) {
                        return Err(bad(format!("no team #{id}")));
                    }
                    format!("team:{id}")
                }
                Some("role") => {
                    let name = s["name"].as_str().ok_or_else(|| bad("share with a role: `name`"))?;
                    if !roles.iter().any(|r| r == name) {
                        return Err(bad(format!("no role `{name}`")));
                    }
                    format!("role:{name}")
                }
                _ => return Err(bad("share `type`: user, team, role or everyone")),
            };
            out.push(Share { target, edit });
        }
        out.sort();
        Ok(out)
    }

    fn shares_json(&mut self, shares: &[Share], names: &HashMap<u64, (String, String)>, teams: &HashMap<u64, String>) -> Vec<J> {
        shares
            .iter()
            .map(|s| match s.target.split_once(':') {
                Some(("user", id)) => {
                    let id: u64 = id.parse().unwrap_or(0);
                    let (login, name) = names.get(&id).cloned().unwrap_or_default();
                    json!({ "type": "user", "id": id, "login": login, "name": name, "edit": s.edit })
                }
                Some(("team", id)) => {
                    let id: u64 = id.parse().unwrap_or(0);
                    json!({ "type": "team", "id": id, "name": teams.get(&id).cloned().unwrap_or_else(|| format!("team #{id} (deleted)")), "edit": s.edit })
                }
                Some(("role", r)) => json!({ "type": "role", "name": r, "edit": s.edit }),
                _ => json!({ "type": "everyone", "edit": s.edit }),
            })
            .collect()
    }

    fn view_json(&mut self, who: &Who, v: &ViewRow, shares: &[Share], pref: Option<&Pref>, names: &HashMap<u64, (String, String)>, teams: &HashMap<u64, String>) -> J {
        let acc = access(who, v, shares).unwrap_or(Access::Use);
        // an app or a view of an interface file is changed in its file (in `quadra dev`, here)
        let file = super::interfaces::file_label(v);
        let read_only = file.is_some() && !self.dev_mode();
        let login = |id: Option<u64>| id.and_then(|i| names.get(&i)).map(|n| n.0.clone());
        let p = pref.cloned().unwrap_or_default();
        json!({
            "id": v.id, "kind": v.kind, "entity": v.entity, "type": v.vtype, "name": v.name, "description": v.description,
            "config": v.config, "version": v.version,
            "owner": { "id": v.owner, "login": login(Some(v.owner)), "name": names.get(&v.owner).map(|n| n.1.clone()) },
            "created_at": v.created_at, "created_by": login(v.created_by), "updated_at": v.updated_at, "updated_by": login(v.updated_by),
            "access": acc.name(), "can_edit": acc >= Access::Edit && !read_only, "can_share": acc >= Access::Admin,
            "origin": v.origin, "file": file, "read_only": read_only,
            "shares": self.shares_json(shares, names, teams),
            "favourite": p.favourite, "default": p.default, "position": p.position,
            "my_filters": p.state.as_ref().map(|s| s["filters"].clone()).filter(|f| !f.is_null()),
            "broken": v.broken, "parent": v.parent,
        })
    }

    fn team_names(&mut self) -> Result<HashMap<u64, String>, Fail> {
        Ok(store::teams(&mut self.conn).map_err(|e| (500, e))?.into_iter().map(|t| (t.id, t.name)).collect())
    }

    /// Views with the names they show.
    fn views_json(&mut self, who: &Who, views: &[ViewRow]) -> Result<Vec<J>, Fail> {
        let ids: Vec<u64> = views.iter().map(|v| v.id).collect();
        let mut shares = store::shares(&mut self.conn, Some(&ids)).map_err(|e| (500, e))?;
        let prefs = store::prefs(&mut self.conn, who.id).map_err(|e| (500, e))?;
        let mut users: Vec<u64> = views.iter().flat_map(|v| [Some(v.owner), v.created_by, v.updated_by]).flatten().collect();
        for s in shares.values().flatten() {
            if let Some(id) = s.target.strip_prefix("user:").and_then(|x| x.parse().ok()) {
                users.push(id);
            }
        }
        let names = self.user_names(&users);
        let teams = self.team_names()?;
        Ok(views.iter().map(|v| self.view_json(who, v, &shares.remove(&v.id).unwrap_or_default(), prefs.get(&v.id), &names, &teams)).collect())
    }

    /// A view the caller may see, with its shares; 404 otherwise (whether it exists is not told).
    pub(super) fn visible(&mut self, who: &Who, id: u64) -> Result<(ViewRow, Vec<Share>, Access), Fail> {
        let v = store::view(&mut self.conn, id).map_err(|e| (500, e))?.filter(|v| !v.deleted).ok_or((404, format!("view {id} not found")))?;
        let shares = store::shares(&mut self.conn, Some(&[id])).map_err(|e| (500, e))?.remove(&id).unwrap_or_default();
        let a = access(who, &v, &shares).ok_or((404, format!("view {id} not found")))?;
        Ok((v, shares, a))
    }

    fn one_view(&mut self, who: &Who, id: u64, status: u16) -> Result<Reply, Fail> {
        let (v, _, _) = self.visible(who, id)?;
        let j = self.views_json(who, &[v])?.pop().unwrap_or(J::Null);
        Ok(reply(status, json!({ "view": j })))
    }

    // ---- folders ----------------------------------------------------------------------------

    /// Every folder (not deleted), by id.
    fn all_folders(&mut self) -> Result<HashMap<u64, ViewRow>, Fail> {
        Ok(store::views(&mut self.conn, Some("folder"), None).map_err(|e| (500, e))?.into_iter().map(|f| (f.id, f)).collect())
    }

    /// May the caller put `item` (an app or a folder, `None`: a new one of `kind` named
    /// `name`) in `parent` (`None`: the top)? The caller may change the folder; a folder does
    /// not go into itself or a folder it holds, nests at most `MAX_DEPTH` deep, and its name is
    /// not one of the folders next to it.
    fn check_place(&mut self, who: &Who, item: Option<&ViewRow>, kind: &str, name: &str, parent: Option<u64>) -> Result<(), Fail> {
        let folders = self.all_folders()?;
        if let Some(p) = parent {
            let Some(f) = folders.get(&p) else {
                return Err((404, format!("folder {p} not found")));
            };
            let shares = store::shares(&mut self.conn, Some(&[p])).map_err(|e| (500, e))?.remove(&p).unwrap_or_default();
            match access(who, f, &shares) {
                None if !self.shown_folders(who, false)?.1.contains(&p) => return Err((404, format!("folder {p} not found"))),
                None => return Err((403, format!("permission denied: folder `{}` is not shared with you, you may not put things in it", f.name))),
                Some(a) if a < Access::Edit => return Err((403, format!("permission denied: folder `{}` is shared with you to use, not to put things in", f.name))),
                Some(_) => {}
            }
        }
        if kind != "folder" {
            return Ok(());
        }
        if let (Some(p), Some(it)) = (parent, item) {
            if ancestry(&folders, p).contains(&it.id) {
                return Err(bad(format!("folder `{}` cannot go into itself or a folder it holds", it.name)));
            }
        }
        let depth = parent.map_or(0, |p| ancestry(&folders, p).len());
        let own = item.map_or(1, |it| height(&folders, it.id));
        if depth + own > MAX_DEPTH {
            return Err(bad(format!("folders nest at most {MAX_DEPTH} deep")));
        }
        let lower = name.to_lowercase();
        if folders.values().any(|f| f.parent == parent && Some(f.id) != item.map(|i| i.id) && f.name.to_lowercase() == lower) {
            let place = parent.and_then(|p| folders.get(&p)).map_or("at the top".to_string(), |f| format!("in `{}`", f.name));
            return Err((409, format!("a folder named `{name}` exists already {place}")));
        }
        Ok(())
    }

    /// A new folder (version 1), owned by the caller.
    fn folder_create(&mut self, who: &Who, body: &J) -> Result<Reply, Fail> {
        if body["type"].as_str().is_some_and(|t| t != "folder") {
            return Err(bad("`type`: a folder is of type `folder`"));
        }
        if !body["config"].is_null() || !body["entity"].is_null() {
            return Err(bad("a folder has no `config` nor `entity`"));
        }
        let name = text_of(body, "name", 100, true)?.unwrap();
        if let Some(m) = folder_name_problem(&name) {
            return Err(bad(m));
        }
        let description = text_of(body, "description", 2000, false)?.unwrap_or_default();
        let parent = parent_of(body)?.flatten();
        self.check_place(who, None, "folder", &name, parent)?;
        let shares = if body["shares"].is_null() { Vec::new() } else { self.parse_shares(&body["shares"])? };
        let row = ViewRow {
            id: 0,
            kind: "folder".into(),
            entity: None,
            vtype: "folder".into(),
            name,
            description,
            config: json!({}),
            owner: who.id,
            version: 1,
            created_at: String::new(),
            created_by: Some(who.id),
            updated_at: String::new(),
            updated_by: Some(who.id),
            deleted: false,
            broken: None,
            checked: Some(self.model_key()),
            renames_seq: 0,
            parent,
            origin: None,
            path: None,
        };
        let id = store::create_view(&mut self.conn, &row, &shares, &at_now(), Some(who.id), "create").map_err(|e| (500, e))?;
        self.one_view(who, id, 201)
    }

    /// Delete a folder: what it holds moves to the folder it was in (refused when a folder it
    /// holds has the name of one there).
    fn folder_delete(&mut self, who: &Who, f: &ViewRow, base: u64) -> Result<Reply, Fail> {
        let folders = self.all_folders()?;
        for c in folders.values().filter(|c| c.parent == Some(f.id)) {
            let lower = c.name.to_lowercase();
            if folders.values().any(|o| o.parent == f.parent && o.id != f.id && o.name.to_lowercase() == lower) {
                let place = f.parent.and_then(|p| folders.get(&p)).map_or("the top".to_string(), |p| format!("`{}`", p.name));
                return Err((409, format!("folder `{}` cannot move up to {place}, where a folder of that name is: rename one of them first", c.name)));
            }
        }
        match store::delete_folder(&mut self.conn, f.id, base, &at_now(), Some(who.id)).map_err(|e| (500, e))? {
            Ok((v, moved)) => Ok(reply(200, json!({ "deleted": f.id, "version": v, "moved": moved.len(), "to": f.parent }))),
            Err(s) => {
                let users: Vec<u64> = s.0.iter().filter_map(|e| e.user).collect();
                let names = self.user_names(&users);
                Ok(stale_reply("folder", s, &names))
            }
        }
    }

    /// The folders shown to the caller: theirs and those shared with them (`direct`), and
    /// those holding (at any depth) an app or a folder they are shown (every folder with
    /// `all`, an admin's). (every folder, shown, direct)
    fn shown_folders(&mut self, who: &Who, all: bool) -> Result<(HashMap<u64, ViewRow>, HashSet<u64>, HashSet<u64>), Fail> {
        let folders = self.all_folders()?;
        let apps = store::views(&mut self.conn, Some("app"), None).map_err(|e| (500, e))?;
        let ids: Vec<u64> = folders.keys().copied().chain(apps.iter().map(|a| a.id)).collect();
        let shares = store::shares(&mut self.conn, Some(&ids)).map_err(|e| (500, e))?;
        let sh = |id: u64| shares.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let direct: HashSet<u64> = folders.values().filter(|f| all || listed(who, f, sh(f.id))).map(|f| f.id).collect();
        let mut shown = direct.clone();
        let holders = apps.iter().filter(|a| all || listed(who, a, sh(a.id))).filter_map(|a| a.parent).chain(direct.iter().filter_map(|f| folders[f].parent));
        for p in holders.collect::<Vec<_>>() {
            shown.extend(ancestry(&folders, p));
        }
        Ok((folders, shown, direct))
    }

    /// The folders shown to the caller, by name, those shown only for what they hold
    /// `implied: true`.
    fn folders_json(&mut self, who: &Who, all: bool) -> Result<Vec<J>, Fail> {
        let (folders, shown, direct) = self.shown_folders(who, all)?;
        let mut rows: Vec<ViewRow> = folders.into_values().filter(|f| shown.contains(&f.id)).collect();
        rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        let mut list = self.views_json(who, &rows)?;
        for j in &mut list {
            j["implied"] = json!(!direct.contains(&j["id"].as_u64().unwrap_or(0)));
        }
        Ok(list)
    }

    // ---- the API ----------------------------------------------------------------------------

    pub(super) fn views_api(&mut self, p: &Principal, method: &str, path: &str, query: &HashMap<String, String>, body: &J) -> Result<Reply, Fail> {
        let segs: Vec<&str> = path.trim_matches('/').split('/').skip(1).collect();
        // a read sent as `POST` (formulas evaluated as the caller): read-only tokens too
        if let ("POST", ["eval"]) = (method, segs.as_slice()) {
            return self.sheet_eval(p, body);
        }
        if let ("POST", ["complete"]) = (method, segs.as_slice()) {
            return self.formula_complete(p, body);
        }
        if method != "GET" && p.read_only {
            return Err((403, "this token is read-only".into()));
        }
        let who = self.who(p)?;
        let id = |s: &str| s.parse::<u64>().map_err(|_| (404, format!("`{s}`: not found")));
        match (method, segs.as_slice()) {
            ("GET", ["views"]) => self.views_list(&who, query),
            ("POST", ["views"]) => self.view_create(&who, body),
            ("PUT", ["views", "order"]) => {
                let ids: Vec<u64> = body["ids"].as_array().ok_or_else(|| bad("`ids`: the views in order"))?.iter().filter_map(|x| x.as_u64()).take(1000).collect();
                store::set_order(&mut self.conn, who.id, &ids).map_err(|e| (500, e))?;
                Ok(reply(200, json!({ "ok": true })))
            }
            ("GET", ["views", v]) => self.one_view(&who, id(v)?, 200),
            ("PUT", ["views", v]) => self.view_update(&who, id(v)?, body),
            ("DELETE", ["views", v]) => {
                let base = query.get("base").and_then(|b| b.parse().ok()).or(body["base"].as_u64()).ok_or_else(|| bad("`base` is required: the version you read"))?;
                self.view_delete(&who, id(v)?, base)
            }
            ("PUT", ["views", v, "shares"]) => self.view_share(&who, id(v)?, body),
            ("PUT", ["views", v, "prefs"]) => {
                let (view, _, _) = self.visible(&who, id(v)?)?;
                // an app: the user's default values of its filters (null: none)
                let state = match body.get("filters") {
                    None => None,
                    Some(J::Null) => Some(None),
                    Some(f @ J::Object(o)) if view.kind == "app" && o.len() <= 50 && f.to_string().len() <= 20_000 => Some(Some(json!({ "filters": f }))),
                    Some(_) => return Err(bad("`filters`: the values of the app's filters by name (an app only, at most 20 KB)")),
                };
                store::set_pref_state(&mut self.conn, who.id, view.id, view.entity.as_deref(), body["favourite"].as_bool(), body["default"].as_bool(), state).map_err(|e| (500, e))?;
                self.one_view(&who, view.id, 200)
            }
            ("GET", ["views", v, "qdi"]) => self.view_qdi(&who, id(v)?),
            ("POST", ["apps", a, "forms", c, "submit"]) => self.form_submit(p, &who, id(a)?, c, body),
            ("GET", ["views", v, "history"]) => {
                let (view, _, _) = self.visible(&who, id(v)?)?;
                self.history_json("view", view.id)
            }
            ("GET", ["teams"]) => self.teams_list(&who),
            ("POST", ["teams"]) => self.team_create(&who, body),
            ("GET", ["teams", t]) => self.one_team(&who, id(t)?, 200),
            ("PUT", ["teams", t]) => self.team_update(&who, id(t)?, body, false),
            ("DELETE", ["teams", t]) => {
                let base = query.get("base").and_then(|b| b.parse().ok()).or(body["base"].as_u64()).ok_or_else(|| bad("`base` is required: the version you read"))?;
                self.team_update(&who, id(t)?, &json!({ "base": base }), true)
            }
            ("GET", ["teams", t, "history"]) => {
                let t = id(t)?;
                store::team(&mut self.conn, t).map_err(|e| (500, e))?.ok_or((404, format!("team {t} not found")))?;
                self.history_json("team", t)
            }
            ("GET", ["directory"]) => self.directory(&who, query),
            ("GET", ["apps", a, "export.xlsx"]) => self.app_export(p, &who, id(a)?, query),
            _ => Err((404, "not found".into())),
        }
    }

    fn views_list(&mut self, who: &Who, query: &HashMap<String, String>) -> Result<Reply, Fail> {
        let kind = query.get("kind").map(String::as_str).unwrap_or("view");
        let entity = query.get("entity").map(String::as_str);
        let all = query.get("scope").map(String::as_str) == Some("all");
        if all && !who.admin {
            return Err((403, "permission denied: only admins list every view".into()));
        }
        let me = json!({ "id": who.id, "admin": who.admin, "roles": who.roles, "teams": who.teams.iter().collect::<Vec<_>>() });
        if kind == "folder" {
            let list = self.folders_json(who, all)?;
            return Ok(reply(200, json!({ "views": list, "admin": who.admin, "user_id": who.id, "me": me })));
        }
        // the user's favourites only (`favourite=1`): the starred views of every entity at once
        let favourites = matches!(query.get("favourite").map(String::as_str), Some("1" | "true"));
        let starred: HashSet<u64> = if favourites { store::prefs(&mut self.conn, who.id).map_err(|e| (500, e))?.into_iter().filter(|(_, p)| p.favourite).map(|(id, _)| id).collect() } else { HashSet::new() };
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        // the entities the caller may read: a view of another one is of no use to them
        let mut readable: HashMap<String, bool> = HashMap::new();
        let rows = store::views(&mut self.conn, Some(kind), entity).map_err(|e| (500, e))?;
        let ids: Vec<u64> = rows.iter().map(|v| v.id).collect();
        let shares = store::shares(&mut self.conn, Some(&ids)).map_err(|e| (500, e))?;
        let mut keep = Vec::new();
        for v in rows {
            let sh = shares.get(&v.id).map(Vec::as_slice).unwrap_or(&[]);
            if !(all || listed(who, &v, sh)) || (favourites && !starred.contains(&v.id)) {
                continue;
            }
            if let Some(e) = &v.entity {
                let ok = match readable.get(e) {
                    Some(b) => *b,
                    None => {
                        let p = Principal { user_id: who.id, read_only: who.read_only, kind: "token".into(), csrf: None, name: String::new() };
                        let b = model.entity(e).is_some() && self.interp(model, Some(&p), None).map(|mut i| i.can("read", e, None, None)).unwrap_or(false);
                        readable.insert(e.clone(), b);
                        b
                    }
                };
                if !ok && !all {
                    continue;
                }
            }
            keep.push(v);
        }
        let mut list = self.views_json(who, &keep)?;
        // the user's order, then by name
        list.sort_by(|a, b| {
            let pa = a["position"].as_i64().unwrap_or(i64::MAX);
            let pb = b["position"].as_i64().unwrap_or(i64::MAX);
            pa.cmp(&pb).then_with(|| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())).then_with(|| a["id"].as_u64().cmp(&b["id"].as_u64()))
        });
        let mut out = json!({ "views": list, "admin": who.admin, "user_id": who.id, "me": me });
        // apps: with the folders they are in, the tree in one answer
        if kind == "app" {
            out["folders"] = json!(self.folders_json(who, all)?);
        }
        Ok(reply(200, out))
    }

    fn view_create(&mut self, who: &Who, body: &J) -> Result<Reply, Fail> {
        let kind = body["kind"].as_str().unwrap_or("view");
        if !KINDS.contains(&kind) {
            return Err(bad(format!("`kind`: {}", KINDS.join(", "))));
        }
        if kind == "app" {
            return self.app_create(who, body);
        }
        if kind == "folder" {
            return self.folder_create(who, body);
        }
        if !body["parent"].is_null() {
            return Err(bad("a saved view of an entity is in no folder (apps and folders are)"));
        }
        let entity = text_of(body, "entity", 200, true)?.unwrap();
        let vtype = body["type"].as_str().unwrap_or("grid");
        if !TYPES.contains(&vtype) {
            return Err(bad(format!("`type`: {}", TYPES.join(", "))));
        }
        let name = text_of(body, "name", 100, true)?.unwrap();
        let description = text_of(body, "description", 2000, false)?.unwrap_or_default();
        let config = if body["config"].is_null() { json!({}) } else { body["config"].clone() };
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        if model.entity(&entity).map_or(true, |e| e.kind != EntityKind::Entity || e.temporary) {
            return Err((404, format!("unknown entity `{entity}`")));
        }
        let p = Principal { user_id: who.id, read_only: false, kind: "token".into(), csrf: None, name: String::new() };
        if !self.interp(model, Some(&p), None)?.can("read", &entity, None, None) {
            return Err((403, format!("permission denied: cannot read {entity}")));
        }
        if let Err(r) = self.valid_config(&entity, &config) {
            return Ok(r);
        }
        let shares = if body["shares"].is_null() { Vec::new() } else { self.parse_shares(&body["shares"])? };
        let renames_seq = store::renames(&mut self.conn).map_err(|e| (500, e))?.last().map(|r| r.0).unwrap_or(0);
        let row = ViewRow {
            id: 0,
            kind: kind.into(),
            entity: Some(entity),
            vtype: vtype.into(),
            name,
            description,
            config,
            owner: who.id,
            version: 1,
            created_at: String::new(),
            created_by: Some(who.id),
            updated_at: String::new(),
            updated_by: Some(who.id),
            deleted: false,
            broken: None,
            checked: Some(self.model_key()),
            renames_seq,
            parent: None,
            origin: None,
            path: None,
        };
        let id = store::create_view(&mut self.conn, &row, &shares, &at_now(), Some(who.id), "create").map_err(|e| (500, e))?;
        self.one_view(who, id, 201)
    }

    fn view_update(&mut self, who: &Who, id: u64, body: &J) -> Result<Reply, Fail> {
        let (v, _, acc) = self.visible(who, id)?;
        let what = match v.kind.as_str() {
            "app" => "app",
            "folder" => "folder",
            _ => "view",
        };
        if acc < Access::Edit {
            return Err((403, format!("permission denied: you may use {what} {id}, not change it (save it as your own)")));
        }
        self.file_guard(&v)?;
        let base = body["base"].as_u64().ok_or_else(|| bad("`base` is required: the version you read"))?;
        let mut change = ViewChange { name: text_of(body, "name", 100, false)?.filter(|n| !n.is_empty()), description: text_of(body, "description", 2000, false)?, ..Default::default() };
        // moved, or a folder renamed: where it goes must take it
        change.parent = parent_of(body)?;
        if change.parent.is_some() && v.kind == "view" {
            return Err(bad("a saved view of an entity is in no folder (apps and folders are)"));
        }
        if v.kind == "folder" {
            if let Some(m) = change.name.as_deref().and_then(folder_name_problem) {
                return Err(bad(m));
            }
        }
        if change.parent.is_some_and(|p| p != v.parent) || (v.kind == "folder" && change.name.as_ref().is_some_and(|n| *n != v.name)) {
            let name = change.name.clone().unwrap_or_else(|| v.name.clone());
            self.check_place(who, Some(&v), &v.kind, &name, change.parent.unwrap_or(v.parent))?;
        }
        if v.kind == "folder" {
            if body["type"].as_str().is_some_and(|t| t != "folder") || !body["config"].is_null() {
                return Err(bad("a folder has a name, a description and a place: no `type` nor `config`"));
            }
        } else if v.kind == "app" {
            if body["type"].as_str().is_some_and(|t| t != "app") {
                return Err(bad("`type`: an app is of type `app`"));
            }
            if !body["config"].is_null() {
                let p = super::apps::app_problems(&body["config"]);
                if !p.is_empty() {
                    return Ok(reply(400, json!({ "error": "the app does not check", "diagnostics": p })));
                }
                change.config = Some(body["config"].clone());
            }
        } else if let Some(t) = body["type"].as_str() {
            if !TYPES.contains(&t) {
                return Err(bad(format!("`type`: {}", TYPES.join(", "))));
            }
            change.vtype = Some(t.into());
        }
        if !body["config"].is_null() && v.kind != "app" {
            let entity = v.entity.clone().unwrap_or_default();
            if let Err(r) = self.valid_config(&entity, &body["config"]) {
                return Ok(r);
            }
            change.config = Some(body["config"].clone());
            // saved against the model running: it fits it
            change.broken = Some(None);
            change.checked = Some(self.model_key());
            change.renames_seq = Some(store::renames(&mut self.conn).map_err(|e| (500, e))?.last().map(|r| r.0).unwrap_or(0).max(v.renames_seq));
        }
        if !body["owner"].is_null() {
            if acc < Access::Admin {
                return Err((403, "permission denied: only the owner or an admin gives a view to someone else".into()));
            }
            let o = body["owner"].as_u64().ok_or_else(|| bad("`owner`: a user id"))?;
            if !self.user_exists(o) {
                return Err(bad(format!("no user #{o}")));
            }
            change.owner = Some(o);
        }
        let moved = change.parent.is_some_and(|p| p != v.parent);
        match store::update_view(&mut self.conn, id, Some(base), &change, &at_now(), Some(who.id), "update").map_err(|e| (500, e))? {
            Ok(_) => {
                // `quadra dev`: the file follows (moved to another folder: to its directory)
                if let Some(path) = v.path.clone().filter(|_| v.origin.as_deref() == Some("file")) {
                    if moved && v.kind == "app" {
                        self.move_file_app(id, &path, change.parent.flatten())?;
                    } else {
                        self.write_back(store::file_of(&path))?;
                    }
                }
                self.one_view(who, id, 200)
            }
            Err(s) => {
                let users: Vec<u64> = s.0.iter().filter_map(|e| e.user).collect();
                let names = self.user_names(&users);
                Ok(stale_reply(what, s, &names))
            }
        }
    }

    /// A new app (version 1), owned by the caller.
    fn app_create(&mut self, who: &Who, body: &J) -> Result<Reply, Fail> {
        if body["type"].as_str().is_some_and(|t| t != "app") {
            return Err(bad("`type`: an app is of type `app`"));
        }
        let name = text_of(body, "name", 100, true)?.unwrap();
        let description = text_of(body, "description", 2000, false)?.unwrap_or_default();
        let config = if body["config"].is_null() { json!({ "version": 1, "pages": [{ "id": "p1", "name": "Page 1", "components": [] }] }) } else { body["config"].clone() };
        let p = super::apps::app_problems(&config);
        if !p.is_empty() {
            return Ok(reply(400, json!({ "error": "the app does not check", "diagnostics": p })));
        }
        let parent = parent_of(body)?.flatten();
        self.check_place(who, None, "app", &name, parent)?;
        let shares = if body["shares"].is_null() { Vec::new() } else { self.parse_shares(&body["shares"])? };
        let row = ViewRow {
            id: 0,
            kind: "app".into(),
            entity: None,
            vtype: "app".into(),
            name,
            description,
            config,
            owner: who.id,
            version: 1,
            created_at: String::new(),
            created_by: Some(who.id),
            updated_at: String::new(),
            updated_by: Some(who.id),
            deleted: false,
            broken: None,
            checked: Some(self.model_key()),
            renames_seq: 0,
            parent,
            origin: None,
            path: None,
        };
        let id = store::create_view(&mut self.conn, &row, &shares, &at_now(), Some(who.id), "create").map_err(|e| (500, e))?;
        self.one_view(who, id, 201)
    }

    fn view_share(&mut self, who: &Who, id: u64, body: &J) -> Result<Reply, Fail> {
        let (v, old, acc) = self.visible(who, id)?;
        if acc < Access::Admin {
            return Err((403, format!("permission denied: only the owner of view {id} or an admin shares it")));
        }
        let base = body["base"].as_u64().ok_or_else(|| bad("`base` is required: the version you read"))?;
        let shares = self.parse_shares(&body["shares"])?;
        self.file_shares_ok(&v, &old, &shares)?;
        let change = ViewChange { shares: Some(shares), ..Default::default() };
        match store::update_view(&mut self.conn, id, Some(base), &change, &at_now(), Some(who.id), "share").map_err(|e| (500, e))? {
            Ok(_) => {
                if let Some(path) = v.path.clone().filter(|_| v.origin.as_deref() == Some("file") && self.dev_mode()) {
                    self.write_back(store::file_of(&path))?;
                }
                self.one_view(who, id, 200)
            }
            Err(s) => {
                let users: Vec<u64> = s.0.iter().filter_map(|e| e.user).collect();
                let names = self.user_names(&users);
                Ok(stale_reply("view", s, &names))
            }
        }
    }

    fn view_delete(&mut self, who: &Who, id: u64, base: u64) -> Result<Reply, Fail> {
        let (v, _, acc) = self.visible(who, id)?;
        if acc < Access::Admin {
            return Err((403, format!("permission denied: only the owner of {} {id} or an admin deletes it", if v.kind == "folder" { "folder" } else { "view" })));
        }
        self.file_guard(&v)?;
        if v.kind == "folder" {
            return self.folder_delete(who, &v, base);
        }
        let change = ViewChange { delete: true, ..Default::default() };
        match store::update_view(&mut self.conn, id, Some(base), &change, &at_now(), Some(who.id), "delete").map_err(|e| (500, e))? {
            Ok(version) => {
                if let Some(path) = v.path.clone().filter(|_| v.origin.as_deref() == Some("file")) {
                    self.write_back(store::file_of(&path))?;
                }
                Ok(reply(200, json!({ "deleted": id, "version": version })))
            }
            Err(s) => {
                let users: Vec<u64> = s.0.iter().filter_map(|e| e.user).collect();
                let names = self.user_names(&users);
                Ok(stale_reply("view", s, &names))
            }
        }
    }

    fn history_json(&mut self, object: &str, id: u64) -> Result<Reply, Fail> {
        let entries = store::log_after(&mut self.conn, object, id, 0).map_err(|e| (500, e))?;
        let users: Vec<u64> = entries.iter().filter_map(|e| e.user).collect();
        let names = self.user_names(&users);
        let list: Vec<J> = entries
            .iter()
            .rev()
            .map(|e| json!({ "version": e.version, "at": e.at, "user": e.user.and_then(|u| names.get(&u)).map(|n| n.0.clone()), "action": e.action, "changed": e.detail["changed"], "state": e.detail }))
            .collect();
        Ok(reply(200, json!({ "entries": list })))
    }

    // ---- teams ----------------------------------------------------------------------------

    fn team_json(&mut self, who: &Who, t: &TeamRow, names: &HashMap<u64, (String, String)>) -> J {
        let owner = t.members.iter().any(|(u, o)| *u == who.id && *o);
        let members: Vec<J> = t
            .members
            .iter()
            .map(|(u, o)| {
                let (login, name) = names.get(u).cloned().unwrap_or_default();
                json!({ "id": u, "login": login, "name": name, "owner": o })
            })
            .collect();
        json!({
            "id": t.id, "name": t.name, "description": t.description, "version": t.version, "members": members,
            "member": t.members.iter().any(|(u, _)| *u == who.id), "can_edit": who.admin || owner,
            "created_at": t.created_at, "updated_at": t.updated_at,
            "updated_by": t.updated_by.and_then(|u| names.get(&u)).map(|n| n.0.clone()),
        })
    }

    fn teams_list(&mut self, who: &Who) -> Result<Reply, Fail> {
        let teams = store::teams(&mut self.conn).map_err(|e| (500, e))?;
        let users: Vec<u64> = teams.iter().flat_map(|t| t.members.iter().map(|m| m.0).chain(t.updated_by)).collect();
        let names = self.user_names(&users);
        let list: Vec<J> = teams.iter().map(|t| self.team_json(who, t, &names)).collect();
        Ok(reply(200, json!({ "teams": list, "admin": who.admin, "can_create": who.admin })))
    }

    fn one_team(&mut self, who: &Who, id: u64, status: u16) -> Result<Reply, Fail> {
        let t = store::team(&mut self.conn, id).map_err(|e| (500, e))?.filter(|t| !t.deleted).ok_or((404, format!("team {id} not found")))?;
        let users: Vec<u64> = t.members.iter().map(|m| m.0).chain(t.updated_by).collect();
        let names = self.user_names(&users);
        Ok(reply(status, json!({ "team": self.team_json(who, &t, &names) })))
    }

    fn parse_members(&mut self, body: &J) -> Result<Vec<(u64, bool)>, Fail> {
        let list = body.as_array().ok_or_else(|| bad("`members`: a list of {id, owner}"))?;
        if list.len() > 5000 {
            return Err(bad("at most 5000 members"));
        }
        let ids: Vec<u64> = list.iter().filter_map(|m| m["id"].as_u64()).collect();
        if ids.len() != list.len() {
            return Err(bad("`members`: each one with the user's `id`"));
        }
        let known = self.user_names(&ids);
        if let Some(u) = ids.iter().find(|u| !known.contains_key(u)) {
            return Err(bad(format!("no user #{u}")));
        }
        Ok(list.iter().map(|m| (m["id"].as_u64().unwrap(), m["owner"].as_bool().unwrap_or(false))).collect())
    }

    fn team_create(&mut self, who: &Who, body: &J) -> Result<Reply, Fail> {
        if !who.admin {
            return Err((403, "permission denied: only admins create teams".into()));
        }
        let name = text_of(body, "name", 100, true)?.unwrap();
        let description = text_of(body, "description", 2000, false)?.unwrap_or_default();
        let mut members = if body["members"].is_null() { Vec::new() } else { self.parse_members(&body["members"])? };
        if store::teams(&mut self.conn).map_err(|e| (500, e))?.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
            return Err((409, format!("a team named `{name}` exists already")));
        }
        if !members.iter().any(|(_, o)| *o) && !members.iter().any(|(u, _)| *u == who.id) {
            // a team has an owner: whoever creates it, unless they named owners
            members.push((who.id, true));
        }
        let id = store::create_team(&mut self.conn, &name, &description, &members, &at_now(), Some(who.id), "create").map_err(|e| (500, e))?;
        self.one_team(who, id, 201)
    }

    fn team_update(&mut self, who: &Who, id: u64, body: &J, delete: bool) -> Result<Reply, Fail> {
        let t = store::team(&mut self.conn, id).map_err(|e| (500, e))?.filter(|t| !t.deleted).ok_or((404, format!("team {id} not found")))?;
        let owner = t.members.iter().any(|(u, o)| *u == who.id && *o);
        if !who.admin && !owner {
            return Err((403, format!("permission denied: only admins and the owners of team `{}` change it", t.name)));
        }
        let base = body["base"].as_u64().ok_or_else(|| bad("`base` is required: the version you read"))?;
        let name = text_of(body, "name", 100, false)?.filter(|n| !n.is_empty());
        if let Some(n) = &name {
            if store::teams(&mut self.conn).map_err(|e| (500, e))?.iter().any(|x| x.id != id && x.name.eq_ignore_ascii_case(n)) {
                return Err((409, format!("a team named `{n}` exists already")));
            }
        }
        let description = text_of(body, "description", 2000, false)?;
        let members = if body["members"].is_null() { None } else { Some(self.parse_members(&body["members"])?) };
        match store::update_team(&mut self.conn, id, base, name.as_deref(), description.as_deref(), members.as_deref(), delete, &at_now(), Some(who.id)).map_err(|e| (500, e))? {
            Ok(v) if delete => Ok(reply(200, json!({ "deleted": id, "version": v }))),
            Ok(_) => self.one_team(who, id, 200),
            Err(s) => {
                let users: Vec<u64> = s.0.iter().filter_map(|e| e.user).collect();
                let names = self.user_names(&users);
                Ok(stale_reply("team", s, &names))
            }
        }
    }

    /// Whom views can be shared with: users (those whose login or name starts with `q`, at
    /// most 50), teams, roles; and who the caller is.
    fn directory(&mut self, who: &Who, query: &HashMap<String, String>) -> Result<Reply, Fail> {
        let q = query.get("q").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
        let mut users = Vec::new();
        if let Some((table, has_name)) = self.users_table() {
            let name = if has_name { "\"name\"" } else { "NULL" };
            let (cond, params) = if q.is_empty() {
                (String::new(), vec![])
            } else {
                let like = format!("{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                let c = if has_name { " WHERE LOWER(\"login\") LIKE ?text ESCAPE '\\' OR LOWER(\"name\") LIKE ?text ESCAPE '\\'" } else { " WHERE LOWER(\"login\") LIKE ?text ESCAPE '\\'" };
                (c.to_string(), if has_name { vec![quadra_store::conn::Sql::Text(like.clone()), quadra_store::conn::Sql::Text(like)] } else { vec![quadra_store::conn::Sql::Text(like)] })
            };
            let rows = self.conn.query(&format!("SELECT CAST(\"id\" AS TEXT), \"login\", {name} FROM \"{table}\"{cond} ORDER BY \"login\" LIMIT 50"), &params).map_err(|e| (500, e))?;
            users = rows.iter().map(|r| json!({ "id": r[0].as_deref().and_then(|x| x.parse::<u64>().ok()), "login": r[1], "name": r[2] })).collect();
        }
        let teams: Vec<J> =
            store::teams(&mut self.conn).map_err(|e| (500, e))?.iter().map(|t| json!({ "id": t.id, "name": t.name, "members": t.members.len(), "member": t.members.iter().any(|(u, _)| *u == who.id) })).collect();
        let roles: Vec<J> = self.checked.model.as_ref().map(|m| m.roles.iter().map(|r| json!({ "name": r.name, "parent": r.parent })).collect()).unwrap_or_default();
        Ok(reply(200, json!({ "users": users, "teams": teams, "roles": roles, "me": { "id": who.id, "admin": who.admin, "roles": who.roles, "teams": who.teams.iter().collect::<Vec<_>>() } })))
    }

    // ---- deploys ----------------------------------------------------------------------------

    /// A view against the model running: the renames given applied to its paths and
    /// expressions, and what still does not fit. (configuration, what was renamed, problems)
    fn fit_view(&mut self, entity: &str, config: &J, renames: &[(String, String, String)]) -> (J, Vec<String>, Vec<String>) {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let report = &checked.report;
        let mut cfg = config.clone();
        let mut notes: Vec<String> = Vec::new();
        let note = |n: String, notes: &mut Vec<String>| {
            if !notes.contains(&n) {
                notes.push(n);
            }
        };
        if !renames.is_empty() && model.entity(entity).is_some() {
            for (_, at, path) in config_paths(&cfg) {
                if let Some(p) = renamed_path(report, entity, &path, renames) {
                    note(format!("{path} → {p}"), &mut notes);
                    set_at(&mut cfg, &at, json!(p));
                }
            }
            for (map, key) in keyed_paths(&cfg) {
                if let Some(p) = renamed_path(report, entity, &key, renames) {
                    let mut cur = &mut cfg;
                    for k in &map {
                        cur = &mut cur[*k];
                    }
                    if let Some(o) = cur.as_object_mut() {
                        if let Some(v) = o.remove(&key) {
                            o.insert(p, v);
                        }
                    }
                }
            }
            // the fields of the model when the view was saved (fields added since are shown)
            if let Some(known) = cfg["known"].as_array().cloned() {
                cfg["known"] = J::Array(known.into_iter().map(|k| k.as_str().and_then(|s| renamed_name(renames, entity, s)).map(J::String).unwrap_or(k)).collect());
            }
            // expressions: the checker says which names no longer resolve; those a rename
            // explains are replaced, and checked again
            if let Ok(mut items) = config_items(&cfg) {
                for _ in 0..8 {
                    let diags = self.check_items(entity, &items);
                    let mut fixes: Vec<(usize, usize, usize, String, String)> = Vec::new();
                    for (k, range, msg) in &diags {
                        let Some((from, to)) = range else { continue };
                        let Some(item) = items.get(*k) else { continue };
                        let fix = if let Some(name) = msg.strip_prefix("unknown name `").and_then(|r| r.split('`').next()) {
                            renamed_name(renames, entity, name).map(|n| (*from, *to, name.to_string(), n, entity.to_string()))
                        } else if let Some((ent, name)) = msg.strip_prefix('`').and_then(|r| r.split_once("` has no field `")).map(|(e, n)| (e.to_string(), n.trim_end_matches('`').to_string())) {
                            renamed_name(renames, &ent, &name).map(|n| (to - name.len(), *to, name.clone(), n, ent.clone()))
                        } else {
                            None
                        };
                        if let Some((f, t, old, new, ent)) = fix {
                            if item.2.get(f..t) == Some(old.as_str()) {
                                fixes.push((*k, f, t, new.clone(), format!("{ent}.{old} → {new}")));
                            }
                        }
                    }
                    if fixes.is_empty() {
                        break;
                    }
                    fixes.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
                    fixes.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
                    for (k, f, t, new, n) in fixes {
                        items[k].2.replace_range(f..t, &new);
                        note(n, &mut notes);
                    }
                }
                put_items(&mut cfg, &items);
            }
        }
        let problems = match self.config_problems(entity, &cfg) {
            Ok(p) => p,
            Err((_, m)) => vec![m],
        };
        (cfg, notes, problems)
    }

    /// After a new model is served (start, deploy): every view checked against another model
    /// is checked against this one, the renames since applied. Returns how many changed.
    pub(super) fn revalidate_views(&mut self) -> usize {
        let key = self.model_key();
        let list = match store::unchecked(&mut self.conn, &key) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("views: {e}");
                return 0;
            }
        };
        if list.is_empty() {
            return 0;
        }
        let renames = store::renames(&mut self.conn).unwrap_or_default();
        let last = renames.last().map(|r| r.0).unwrap_or(0);
        let at = at_now();
        let mut changed = 0;
        for v in list {
            // apps check their formulas when they are evaluated; the views of interface files
            // are checked with the code (`quadra check`): renames are made in the files
            if v.kind != "view" || v.origin.as_deref() == Some("file") {
                continue;
            }
            let pending: Vec<(String, String, String)> = renames.iter().filter(|r| r.0 > v.renames_seq).map(|r| (r.1.clone(), r.2.clone(), r.3.clone())).collect();
            let entity = v.entity.clone().unwrap_or_default();
            let (config, notes, problems) = self.fit_view(&entity, &v.config, &pending);
            let broken = (!problems.is_empty()).then(|| json!(problems));
            let action = if !notes.is_empty() {
                format!("deploy: renamed {}", notes.join(", "))
            } else if broken.is_some() && broken != v.broken {
                "deploy: no longer fits the model".to_string()
            } else if broken.is_none() && v.broken.is_some() {
                "deploy: fits the model again".to_string()
            } else {
                "deploy".to_string()
            };
            let change = ViewChange { config: (config != v.config).then_some(config), broken: Some(broken), checked: Some(key.clone()), renames_seq: Some(last.max(v.renames_seq)), ..Default::default() };
            match store::update_view(&mut self.conn, v.id, None, &change, &at, None, &action) {
                Ok(Ok(n)) if n != v.version => changed += 1,
                Ok(_) => {}
                Err(e) => eprintln!("views: view {}: {e}", v.id),
            }
        }
        if changed > 0 {
            eprintln!("views: {changed} saved view(s) updated for the new model");
        }
        changed
    }
}
