//! What users make in the web client and share: saved views and boards of an entity (later
//! dashboards, `kind` tells them apart), and teams to share them with. Data, not code: they
//! live in system tables next to the records, not in the model, so they survive deploys and
//! never appear among the entities.
//!
//! - every change is one database transaction, recorded in `_quadra_view_log` (who, when,
//!   what, the state after it) under the object's new version;
//! - a change is made from the version it was read at (`base`): if someone changed the object
//!   since, nothing is written and the caller gets the changes made meanwhile (a 409, as for
//!   records);
//! - a deleted view or team is marked, never removed: its history stays;
//! - apps and folders are in a folder (`parent`) or at the top: folders nest, a tree.
//!
//! Who may see or change what is decided by the server (`quadra-server`, engine/views.rs):
//! this module only stores.

use crate::conn::{Conn, Dialect, Sql};
use serde_json::{json, Value as J};
use std::collections::HashMap;

/// A saved view, an app, or a folder of apps.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRow {
    pub id: u64,
    /// `view`, `app` or `folder`
    pub kind: String,
    /// the entity a view shows (none for a dashboard)
    pub entity: Option<String>,
    /// `grid`, `groups`, `kanban`
    pub vtype: String,
    pub name: String,
    pub description: String,
    /// what the client shows: columns, filter, order, groups, board… (JSON)
    pub config: J,
    pub owner: u64,
    pub version: u64,
    pub created_at: String,
    pub created_by: Option<u64>,
    pub updated_at: String,
    pub updated_by: Option<u64>,
    pub deleted: bool,
    /// what no longer fits the model since a deploy: the checker's messages
    pub broken: Option<J>,
    /// the model the view was last checked against
    pub checked: Option<String>,
    /// the last field rename (`_quadra_renames.seq`) applied to it
    pub renames_seq: u64,
    /// the folder an app or a folder is in (none: at the top)
    pub parent: Option<u64>,
    /// `file`: installed from an interface file of the project (`None`: made in the client)
    pub origin: Option<String>,
    /// its identity in the project's `interfaces/` directory: `sales/cockpit` (an app),
    /// `sales/orders#To ship` (a view), `sales` (a folder)
    pub path: Option<String>,
}

/// Who a view is shared with: `user:<id>`, `team:<id>`, `role:<name>`, `everyone`; `edit`:
/// they may change it too (co-owners).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Share {
    pub target: String,
    pub edit: bool,
}

/// A user's own settings of a view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pref {
    pub favourite: bool,
    pub position: Option<i64>,
    pub default: bool,
    /// the user's own state of the view (an app: the default values of its filters)
    pub state: Option<J>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TeamRow {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub version: u64,
    pub created_at: String,
    pub created_by: Option<u64>,
    pub updated_at: String,
    pub updated_by: Option<u64>,
    pub deleted: bool,
    /// (user, owner of the team)
    pub members: Vec<(u64, bool)>,
}

/// A change of a view or a team, as recorded.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub version: u64,
    pub at: String,
    pub user: Option<u64>,
    pub action: String,
    /// what changed, and the state after the change
    pub detail: J,
}

/// What a change of a view sets (`None`: unchanged).
#[derive(Clone, Debug, Default)]
pub struct ViewChange {
    pub name: Option<String>,
    pub description: Option<String>,
    pub vtype: Option<String>,
    pub config: Option<J>,
    pub owner: Option<u64>,
    pub shares: Option<Vec<Share>>,
    /// `Some(None)`: no longer broken
    pub broken: Option<Option<J>>,
    pub checked: Option<String>,
    pub renames_seq: Option<u64>,
    /// moved to another folder (`Some(None)`: to the top)
    pub parent: Option<Option<u64>>,
    pub delete: bool,
}

/// The object changed since `base`: the changes made meanwhile, nothing written.
#[derive(Clone, Debug)]
pub struct Stale(pub Vec<LogEntry>);

fn num(s: &Option<String>) -> u64 {
    s.as_deref()
        .and_then(|x| x.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0) as u64
}

fn opt_num(s: &Option<String>) -> Option<u64> {
    s.as_deref()
        .and_then(|x| x.parse::<i64>().ok())
        .map(|v| v.max(0) as u64)
}

/// A boolean as both databases print it (`1`/`0`, `true`/`false`).
fn truth(s: &Option<String>) -> bool {
    matches!(s.as_deref(), Some("1" | "true" | "t"))
}

fn text(s: &Option<String>) -> String {
    s.clone().unwrap_or_default()
}

fn opt_int(v: Option<u64>) -> Sql {
    v.map(|x| Sql::Int(x as i64)).unwrap_or(Sql::Null)
}

/// One transaction of these tables: serialized with the other writers of views and teams
/// (`BEGIN IMMEDIATE` on SQLite, a table lock on PostgreSQL), so that new ids and versions
/// never collide, across processes too.
pub fn write<T>(
    conn: &mut Conn,
    f: impl FnOnce(&mut Conn) -> Result<T, String>,
) -> Result<T, String> {
    match conn.dialect() {
        Dialect::Sqlite => conn.batch("BEGIN IMMEDIATE")?,
        Dialect::Postgres => conn.batch(
            "BEGIN; LOCK TABLE \"_quadra_views\", \"_quadra_teams\" IN SHARE ROW EXCLUSIVE MODE",
        )?,
    }
    match f(conn) {
        Ok(v) => {
            conn.batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---- views --------------------------------------------------------------------------------

const VIEW_COLS: &str = "CAST(\"id\" AS TEXT), \"kind\", \"entity\", \"type\", \"name\", \"description\", \"config\", CAST(\"owner_id\" AS TEXT), CAST(\"version\" AS TEXT), \"created_at\", CAST(\"created_by\" AS TEXT), \"updated_at\", CAST(\"updated_by\" AS TEXT), \"deleted_at\", \"broken\", \"checked\", CAST(\"renames_seq\" AS TEXT), CAST(\"parent_id\" AS TEXT), \"origin\", \"path\"";

fn view_of(r: &[Option<String>]) -> ViewRow {
    ViewRow {
        id: num(&r[0]),
        kind: text(&r[1]),
        entity: r[2].clone(),
        vtype: text(&r[3]),
        name: text(&r[4]),
        description: text(&r[5]),
        config: serde_json::from_str(r[6].as_deref().unwrap_or("{}")).unwrap_or(json!({})),
        owner: num(&r[7]),
        version: num(&r[8]),
        created_at: text(&r[9]),
        created_by: opt_num(&r[10]),
        updated_at: text(&r[11]),
        updated_by: opt_num(&r[12]),
        deleted: r[13].is_some(),
        broken: r[14].as_deref().and_then(|b| serde_json::from_str(b).ok()),
        checked: r[15].clone(),
        renames_seq: num(&r[16]),
        parent: opt_num(&r[17]),
        origin: r.get(18).cloned().flatten(),
        path: r.get(19).cloned().flatten(),
    }
}

/// The views that are not deleted, of a kind and an entity when given, by id.
pub fn views(
    conn: &mut Conn,
    kind: Option<&str>,
    entity: Option<&str>,
) -> Result<Vec<ViewRow>, String> {
    let mut sql = format!("SELECT {VIEW_COLS} FROM \"_quadra_views\" WHERE \"deleted_at\" IS NULL");
    let mut params = Vec::new();
    if let Some(k) = kind {
        sql.push_str(" AND \"kind\" = ?text");
        params.push(Sql::Text(k.into()));
    }
    if let Some(e) = entity {
        sql.push_str(" AND \"entity\" = ?text");
        params.push(Sql::Text(e.into()));
    }
    sql.push_str(" ORDER BY \"_quadra_views\".\"id\"");
    Ok(conn
        .query(&sql, &params)?
        .iter()
        .map(|r| view_of(r))
        .collect())
}

/// A view, deleted or not.
pub fn view(conn: &mut Conn, id: u64) -> Result<Option<ViewRow>, String> {
    Ok(conn
        .query(
            &format!("SELECT {VIEW_COLS} FROM \"_quadra_views\" WHERE \"id\" = ?bigint"),
            &[Sql::Int(id as i64)],
        )?
        .first()
        .map(|r| view_of(r)))
}

/// The shares of views (all of them when `ids` is `None`), by view, in a stable order.
pub fn shares(conn: &mut Conn, ids: Option<&[u64]>) -> Result<HashMap<u64, Vec<Share>>, String> {
    let mut sql = "SELECT CAST(\"view_id\" AS TEXT), \"target\", CAST(\"can_edit\" AS TEXT) FROM \"_quadra_view_shares\"".to_string();
    if let Some(ids) = ids {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        sql.push_str(&format!(
            " WHERE \"view_id\" IN ({})",
            ids.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut out: HashMap<u64, Vec<Share>> = HashMap::new();
    for r in conn.query(&sql, &[])? {
        out.entry(num(&r[0])).or_default().push(Share {
            target: text(&r[1]),
            edit: truth(&r[2]),
        });
    }
    for v in out.values_mut() {
        v.sort();
    }
    Ok(out)
}

/// A user's settings of their views.
pub fn prefs(conn: &mut Conn, user: u64) -> Result<HashMap<u64, Pref>, String> {
    let rows = conn.query(
        "SELECT CAST(\"view_id\" AS TEXT), CAST(\"favourite\" AS TEXT), CAST(\"position\" AS TEXT), CAST(\"is_default\" AS TEXT), \"state\" FROM \"_quadra_view_prefs\" WHERE \"user_id\" = ?bigint",
        &[Sql::Int(user as i64)],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                num(&r[0]),
                Pref {
                    favourite: truth(&r[1]),
                    position: r[2].as_deref().and_then(|p| p.parse().ok()),
                    default: truth(&r[3]),
                    state: r[4].as_deref().and_then(|s| serde_json::from_str(s).ok()),
                },
            )
        })
        .collect())
}

fn next_id(conn: &mut Conn, table: &str) -> Result<u64, String> {
    let r = conn.query(
        &format!("SELECT CAST(COALESCE(MAX(\"id\"), 0) + 1 AS TEXT) FROM \"{table}\""),
        &[],
    )?;
    Ok(r.first().map(|r| num(&r[0])).unwrap_or(1).max(1))
}

fn log(
    conn: &mut Conn,
    object: &str,
    id: u64,
    version: u64,
    at: &str,
    user: Option<u64>,
    action: &str,
    detail: &J,
) -> Result<(), String> {
    conn.exec(
        "INSERT INTO \"_quadra_view_log\" (\"object\", \"object_id\", \"version\", \"at\", \"user_id\", \"action\", \"detail\") VALUES (?text, ?bigint, ?bigint, ?text, ?bigint, ?text, ?text)",
        &[Sql::Text(object.into()), Sql::Int(id as i64), Sql::Int(version as i64), Sql::Text(at.into()), opt_int(user), Sql::Text(action.into()), Sql::Text(detail.to_string())],
    )?;
    Ok(())
}

fn set_shares(conn: &mut Conn, id: u64, shares: &[Share]) -> Result<(), String> {
    conn.exec(
        "DELETE FROM \"_quadra_view_shares\" WHERE \"view_id\" = ?bigint",
        &[Sql::Int(id as i64)],
    )?;
    let mut seen = std::collections::HashSet::new();
    for s in shares {
        if !seen.insert(s.target.clone()) {
            continue;
        }
        conn.exec("INSERT INTO \"_quadra_view_shares\" (\"view_id\", \"target\", \"can_edit\") VALUES (?bigint, ?text, ?boolean)", &[Sql::Int(id as i64), Sql::Text(s.target.clone()), Sql::Bool(s.edit)])?;
    }
    Ok(())
}

/// The state of a view as the log keeps it.
pub fn view_state(v: &ViewRow, shares: &[Share]) -> J {
    json!({
        "kind": v.kind, "entity": v.entity, "type": v.vtype, "name": v.name, "description": v.description,
        "config": v.config, "owner": v.owner, "deleted": v.deleted, "broken": v.broken, "parent": v.parent,
        "shares": shares.iter().map(|s| json!({ "target": s.target, "edit": s.edit })).collect::<Vec<_>>(),
    })
}

/// A new view (version 1); its id. `action` is what the log says (`create`, `import`…).
pub fn create_view(
    conn: &mut Conn,
    v: &ViewRow,
    shares: &[Share],
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<u64, String> {
    write(conn, |conn| insert_view(conn, v, shares, at, user, action))
}

/// `create_view` inside a transaction of the caller's (`write`).
pub fn insert_view(
    conn: &mut Conn,
    v: &ViewRow,
    shares: &[Share],
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<u64, String> {
    {
        let id = next_id(conn, "_quadra_views")?;
        conn.exec(
            "INSERT INTO \"_quadra_views\" (\"id\", \"kind\", \"entity\", \"type\", \"name\", \"description\", \"config\", \"owner_id\", \"version\", \"created_at\", \"created_by\", \"updated_at\", \"updated_by\", \"broken\", \"checked\", \"renames_seq\", \"parent_id\", \"origin\", \"path\") VALUES (?bigint, ?text, ?text, ?text, ?text, ?text, ?text, ?bigint, 1, ?text, ?bigint, ?text, ?bigint, ?text, ?text, ?bigint, ?bigint, ?text, ?text)",
            &[
                Sql::Int(id as i64),
                Sql::Text(v.kind.clone()),
                v.entity.clone().map(Sql::Text).unwrap_or(Sql::Null),
                Sql::Text(v.vtype.clone()),
                Sql::Text(v.name.clone()),
                Sql::Text(v.description.clone()),
                Sql::Text(v.config.to_string()),
                Sql::Int(v.owner as i64),
                Sql::Text(at.into()),
                opt_int(user),
                Sql::Text(at.into()),
                opt_int(user),
                v.broken.as_ref().map(|b| Sql::Text(b.to_string())).unwrap_or(Sql::Null),
                v.checked.clone().map(Sql::Text).unwrap_or(Sql::Null),
                Sql::Int(v.renames_seq as i64),
                opt_int(v.parent),
                v.origin.clone().map(Sql::Text).unwrap_or(Sql::Null),
                v.path.clone().map(Sql::Text).unwrap_or(Sql::Null),
            ],
        )?;
        set_shares(conn, id, shares)?;
        let mut state = view_state(v, shares);
        state["changed"] = json!(["created"]);
        log(conn, "view", id, 1, at, user, action, &state)?;
        Ok(id)
    }
}

/// The log of an object after a version, oldest first.
pub fn log_after(
    conn: &mut Conn,
    object: &str,
    id: u64,
    after: u64,
) -> Result<Vec<LogEntry>, String> {
    let rows = conn.query(
        "SELECT CAST(\"version\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT), \"action\", \"detail\" FROM \"_quadra_view_log\" WHERE \"object\" = ?text AND \"object_id\" = ?bigint AND \"version\" > ?bigint ORDER BY \"version\"",
        &[Sql::Text(object.into()), Sql::Int(id as i64), Sql::Int(after as i64)],
    )?;
    Ok(rows
        .iter()
        .map(|r| LogEntry {
            version: num(&r[0]),
            at: text(&r[1]),
            user: opt_num(&r[2]),
            action: text(&r[3]),
            detail: serde_json::from_str(r[4].as_deref().unwrap_or("{}")).unwrap_or(J::Null),
        })
        .collect())
}

/// What differs between two states of a view: `name`, `config.where`, `shares`…
fn view_diff(before: &J, after: &J) -> Vec<String> {
    let mut out = Vec::new();
    for k in [
        "name",
        "description",
        "type",
        "owner",
        "shares",
        "deleted",
        "broken",
        "entity",
        "parent",
    ] {
        if before[k] != after[k] {
            out.push(k.to_string());
        }
    }
    let (a, b) = (before["config"].as_object(), after["config"].as_object());
    let mut keys: Vec<&String> = a.map(|o| o.keys().collect()).unwrap_or_default();
    for k in b.map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default() {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys.sort();
    for k in keys {
        if before["config"][k] != after["config"][k] {
            out.push(format!("config.{k}"));
        }
    }
    out
}

/// Change a view read at version `base` (`None`: whatever its version, for the system: a
/// deploy). Returns the new version, or `Stale` with the changes made since `base`.
pub fn update_view(
    conn: &mut Conn,
    id: u64,
    base: Option<u64>,
    change: &ViewChange,
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<Result<u64, Stale>, String> {
    write(conn, |conn| {
        change_view(conn, id, base, change, at, user, action)
    })
}

/// `update_view` inside a transaction of the caller's (`write`).
pub fn change_view(
    conn: &mut Conn,
    id: u64,
    base: Option<u64>,
    change: &ViewChange,
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<Result<u64, Stale>, String> {
    {
        let Some(mut v) = view(conn, id)? else {
            return Err(format!("view {id} not found"));
        };
        if let Some(b) = base {
            if b != v.version {
                // moves to another folder since `base`, when this change does not move it, are not
                // a conflict (an app moved while it is being edited)
                let since = log_after(conn, "view", id, b)?;
                let mut set: Vec<&str> = Vec::new();
                if change.name.is_some() {
                    set.push("name");
                }
                if change.description.is_some() {
                    set.push("description");
                }
                if change.shares.is_some() {
                    set.push("shares");
                }
                if change.owner.is_some() {
                    set.push("owner");
                }
                if change.parent.is_some() {
                    set.push("parent");
                }
                if change.config.is_some() || change.vtype.is_some() {
                    set.push("config");
                    set.push("type");
                }
                let apart = |k: &str| k == "parent" && !set.contains(&k);
                let independent = !since.is_empty()
                    && !change.delete
                    && since.iter().all(|e| {
                        e.detail["changed"]
                            .as_array()
                            .is_some_and(|c| c.iter().all(|k| k.as_str().is_some_and(apart)))
                    });
                if !independent {
                    return Ok(Err(Stale(since)));
                }
            }
        }
        let old_shares = shares(conn, Some(&[id]))?.remove(&id).unwrap_or_default();
        let before = view_state(&v, &old_shares);
        if let Some(n) = &change.name {
            v.name = n.clone();
        }
        if let Some(d) = &change.description {
            v.description = d.clone();
        }
        if let Some(t) = &change.vtype {
            v.vtype = t.clone();
        }
        if let Some(c) = &change.config {
            v.config = c.clone();
        }
        if let Some(o) = change.owner {
            v.owner = o;
        }
        if let Some(b) = &change.broken {
            v.broken = b.clone();
        }
        if let Some(c) = &change.checked {
            v.checked = Some(c.clone());
        }
        if let Some(s) = change.renames_seq {
            v.renames_seq = s;
        }
        if let Some(p) = change.parent {
            v.parent = p;
        }
        if change.delete {
            v.deleted = true;
        }
        let new_shares = change.shares.clone().unwrap_or(old_shares);
        let mut after = view_state(&v, &new_shares);
        let changed = view_diff(&before, &after);
        if changed.is_empty() {
            // the bookkeeping of the checks only: not a new version
            conn.exec(
                "UPDATE \"_quadra_views\" SET \"checked\" = ?text, \"renames_seq\" = ?bigint WHERE \"id\" = ?bigint",
                &[v.checked.clone().map(Sql::Text).unwrap_or(Sql::Null), Sql::Int(v.renames_seq as i64), Sql::Int(id as i64)],
            )?;
            return Ok(Ok(v.version));
        }
        let version = v.version + 1;
        conn.exec(
            "UPDATE \"_quadra_views\" SET \"type\" = ?text, \"name\" = ?text, \"description\" = ?text, \"config\" = ?text, \"owner_id\" = ?bigint, \"version\" = ?bigint, \"updated_at\" = ?text, \"updated_by\" = ?bigint, \"deleted_at\" = ?text, \"deleted_by\" = ?bigint, \"broken\" = ?text, \"checked\" = ?text, \"renames_seq\" = ?bigint, \"parent_id\" = ?bigint WHERE \"id\" = ?bigint",
            &[
                Sql::Text(v.vtype.clone()),
                Sql::Text(v.name.clone()),
                Sql::Text(v.description.clone()),
                Sql::Text(v.config.to_string()),
                Sql::Int(v.owner as i64),
                Sql::Int(version as i64),
                Sql::Text(at.into()),
                opt_int(user),
                if v.deleted { Sql::Text(at.into()) } else { Sql::Null },
                if v.deleted { opt_int(user) } else { Sql::Null },
                v.broken.as_ref().map(|b| Sql::Text(b.to_string())).unwrap_or(Sql::Null),
                v.checked.clone().map(Sql::Text).unwrap_or(Sql::Null),
                Sql::Int(v.renames_seq as i64),
                opt_int(v.parent),
                Sql::Int(id as i64),
            ],
        )?;
        if change.shares.is_some() {
            set_shares(conn, id, &new_shares)?;
        }
        after["changed"] = json!(changed);
        log(conn, "view", id, version, at, user, action, &after)?;
        Ok(Ok(version))
    }
}

/// Delete a folder read at version `base`: what is in it (apps and folders, deleted ones
/// aside, whoever may see them) moves to the folder it was in, each a new version of its own
/// (`move`, recorded in its history). One transaction. Returns the folder's new version and
/// what moved, or `Stale`.
pub fn delete_folder(
    conn: &mut Conn,
    id: u64,
    base: u64,
    at: &str,
    user: Option<u64>,
) -> Result<Result<(u64, Vec<u64>), Stale>, String> {
    write(conn, |conn| {
        let Some(f) = view(conn, id)? else {
            return Err(format!("folder {id} not found"));
        };
        if f.version != base {
            return Ok(Err(Stale(log_after(conn, "view", id, base)?)));
        }
        let moved: Vec<u64> = children(conn, id)?.iter().map(|c| c.id).collect();
        for c in &moved {
            let change = ViewChange {
                parent: Some(f.parent),
                ..Default::default()
            };
            // no base: never stale
            let _ = change_view(
                conn,
                *c,
                None,
                &change,
                at,
                user,
                "move: its folder was deleted",
            )?;
        }
        let change = ViewChange {
            delete: true,
            ..Default::default()
        };
        match change_view(conn, id, Some(base), &change, at, user, "delete")? {
            Ok(v) => Ok(Ok((v, moved))),
            Err(s) => Ok(Err(s)),
        }
    })
}

/// What a folder holds directly (not deleted), by id.
pub fn children(conn: &mut Conn, id: u64) -> Result<Vec<ViewRow>, String> {
    Ok(conn
        .query(&format!("SELECT {VIEW_COLS} FROM \"_quadra_views\" WHERE \"deleted_at\" IS NULL AND \"parent_id\" = ?bigint ORDER BY \"_quadra_views\".\"id\""), &[Sql::Int(id as i64)])?
        .iter()
        .map(|r| view_of(r))
        .collect())
}

/// A user's favourite and default (the default of an entity is one view: the others of the
/// same entity stop being it).
pub fn set_pref(
    conn: &mut Conn,
    user: u64,
    id: u64,
    entity: Option<&str>,
    favourite: Option<bool>,
    default: Option<bool>,
) -> Result<(), String> {
    set_pref_state(conn, user, id, entity, favourite, default, None)
}

/// `set_pref`, and the user's own state of the view (`Some(None)`: forget it).
pub fn set_pref_state(
    conn: &mut Conn,
    user: u64,
    id: u64,
    entity: Option<&str>,
    favourite: Option<bool>,
    default: Option<bool>,
    state: Option<Option<J>>,
) -> Result<(), String> {
    write(conn, |conn| {
        let mut p = prefs(conn, user)?.remove(&id).unwrap_or_default();
        if let Some(s) = state {
            p.state = s;
        }
        if let Some(f) = favourite {
            p.favourite = f;
        }
        if let Some(d) = default {
            if d {
                if let Some(e) = entity {
                    conn.exec(
                        "UPDATE \"_quadra_view_prefs\" SET \"is_default\" = ?boolean WHERE \"user_id\" = ?bigint AND \"view_id\" IN (SELECT \"id\" FROM \"_quadra_views\" WHERE \"entity\" = ?text)",
                        &[Sql::Bool(false), Sql::Int(user as i64), Sql::Text(e.into())],
                    )?;
                }
            }
            p.default = d;
        }
        upsert_pref(conn, user, id, &p)
    })
}

fn upsert_pref(conn: &mut Conn, user: u64, id: u64, p: &Pref) -> Result<(), String> {
    conn.exec(
        "INSERT INTO \"_quadra_view_prefs\" (\"user_id\", \"view_id\", \"favourite\", \"position\", \"is_default\", \"state\") VALUES (?bigint, ?bigint, ?boolean, ?bigint, ?boolean, ?text) ON CONFLICT (\"user_id\", \"view_id\") DO UPDATE SET \"favourite\" = excluded.\"favourite\", \"position\" = excluded.\"position\", \"is_default\" = excluded.\"is_default\", \"state\" = excluded.\"state\"",
        &[Sql::Int(user as i64), Sql::Int(id as i64), Sql::Bool(p.favourite), p.position.map(Sql::Int).unwrap_or(Sql::Null), Sql::Bool(p.default), p.state.as_ref().map(|s| Sql::Text(s.to_string())).unwrap_or(Sql::Null)],
    )?;
    Ok(())
}

/// A user's order of views (in the switcher): the position of each, in the order given.
pub fn set_order(conn: &mut Conn, user: u64, ids: &[u64]) -> Result<(), String> {
    write(conn, |conn| {
        let mut all = prefs(conn, user)?;
        for (n, id) in ids.iter().enumerate() {
            let mut p = all.remove(id).unwrap_or_default();
            p.position = Some(n as i64);
            upsert_pref(conn, user, *id, &p)?;
        }
        Ok(())
    })
}

// ---- teams --------------------------------------------------------------------------------

const TEAM_COLS: &str = "CAST(\"id\" AS TEXT), \"name\", \"description\", CAST(\"version\" AS TEXT), \"created_at\", CAST(\"created_by\" AS TEXT), \"updated_at\", CAST(\"updated_by\" AS TEXT), \"deleted_at\"";

fn members_of(conn: &mut Conn, ids: &[u64]) -> Result<HashMap<u64, Vec<(u64, bool)>>, String> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = conn.query(
        &format!(
            "SELECT CAST(\"team_id\" AS TEXT), CAST(\"user_id\" AS TEXT), CAST(\"owner\" AS TEXT) FROM \"_quadra_team_members\" WHERE \"team_id\" IN ({}) ORDER BY \"team_id\", \"user_id\"",
            ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        ),
        &[],
    )?;
    let mut out: HashMap<u64, Vec<(u64, bool)>> = HashMap::new();
    for r in rows {
        out.entry(num(&r[0]))
            .or_default()
            .push((num(&r[1]), truth(&r[2])));
    }
    Ok(out)
}

fn team_of(r: &[Option<String>]) -> TeamRow {
    TeamRow {
        id: num(&r[0]),
        name: text(&r[1]),
        description: text(&r[2]),
        version: num(&r[3]),
        created_at: text(&r[4]),
        created_by: opt_num(&r[5]),
        updated_at: text(&r[6]),
        updated_by: opt_num(&r[7]),
        deleted: r[8].is_some(),
        members: Vec::new(),
    }
}

/// The teams that are not deleted, by name, with their members.
pub fn teams(conn: &mut Conn) -> Result<Vec<TeamRow>, String> {
    let mut out: Vec<TeamRow> = conn.query(&format!("SELECT {TEAM_COLS} FROM \"_quadra_teams\" WHERE \"deleted_at\" IS NULL ORDER BY \"name\", \"id\""), &[])?.iter().map(|r| team_of(r)).collect();
    let ids: Vec<u64> = out.iter().map(|t| t.id).collect();
    let mut members = members_of(conn, &ids)?;
    for t in &mut out {
        t.members = members.remove(&t.id).unwrap_or_default();
    }
    Ok(out)
}

/// A team, deleted or not.
pub fn team(conn: &mut Conn, id: u64) -> Result<Option<TeamRow>, String> {
    let Some(mut t) = conn
        .query(
            &format!("SELECT {TEAM_COLS} FROM \"_quadra_teams\" WHERE \"id\" = ?bigint"),
            &[Sql::Int(id as i64)],
        )?
        .first()
        .map(|r| team_of(r))
    else {
        return Ok(None);
    };
    t.members = members_of(conn, &[id])?.remove(&id).unwrap_or_default();
    Ok(Some(t))
}

/// The teams (not deleted) a user is a member of.
pub fn teams_of(conn: &mut Conn, user: u64) -> Result<Vec<u64>, String> {
    let rows = conn.query(
        "SELECT CAST(m.\"team_id\" AS TEXT) FROM \"_quadra_team_members\" m JOIN \"_quadra_teams\" t ON t.\"id\" = m.\"team_id\" WHERE m.\"user_id\" = ?bigint AND t.\"deleted_at\" IS NULL",
        &[Sql::Int(user as i64)],
    )?;
    Ok(rows.iter().map(|r| num(&r[0])).collect())
}

pub fn team_state(t: &TeamRow) -> J {
    json!({
        "name": t.name, "description": t.description, "deleted": t.deleted,
        "members": t.members.iter().map(|(u, o)| json!({ "user": u, "owner": o })).collect::<Vec<_>>(),
    })
}

fn set_members(conn: &mut Conn, id: u64, members: &[(u64, bool)]) -> Result<(), String> {
    conn.exec(
        "DELETE FROM \"_quadra_team_members\" WHERE \"team_id\" = ?bigint",
        &[Sql::Int(id as i64)],
    )?;
    let mut seen = std::collections::HashSet::new();
    for (u, o) in members {
        if !seen.insert(*u) {
            continue;
        }
        conn.exec("INSERT INTO \"_quadra_team_members\" (\"team_id\", \"user_id\", \"owner\") VALUES (?bigint, ?bigint, ?boolean)", &[Sql::Int(id as i64), Sql::Int(*u as i64), Sql::Bool(*o)])?;
    }
    Ok(())
}

/// A new team (version 1); its id.
pub fn create_team(
    conn: &mut Conn,
    name: &str,
    description: &str,
    members: &[(u64, bool)],
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<u64, String> {
    write(conn, |conn| {
        insert_team(conn, name, description, members, at, user, action)
    })
}

/// `create_team` inside a transaction of the caller's (`write`).
pub fn insert_team(
    conn: &mut Conn,
    name: &str,
    description: &str,
    members: &[(u64, bool)],
    at: &str,
    user: Option<u64>,
    action: &str,
) -> Result<u64, String> {
    {
        let id = next_id(conn, "_quadra_teams")?;
        conn.exec(
            "INSERT INTO \"_quadra_teams\" (\"id\", \"name\", \"description\", \"version\", \"created_at\", \"created_by\", \"updated_at\", \"updated_by\") VALUES (?bigint, ?text, ?text, 1, ?text, ?bigint, ?text, ?bigint)",
            &[Sql::Int(id as i64), Sql::Text(name.into()), Sql::Text(description.into()), Sql::Text(at.into()), opt_int(user), Sql::Text(at.into()), opt_int(user)],
        )?;
        set_members(conn, id, members)?;
        let t = TeamRow {
            id,
            name: name.into(),
            description: description.into(),
            version: 1,
            created_at: at.into(),
            created_by: user,
            updated_at: at.into(),
            updated_by: user,
            deleted: false,
            members: members.to_vec(),
        };
        let mut state = team_state(&t);
        state["changed"] = json!(["created"]);
        log(conn, "team", id, 1, at, user, action, &state)?;
        Ok(id)
    }
}

/// Change a team read at version `base`: its name, description, members, or delete it.
#[allow(clippy::too_many_arguments)]
pub fn update_team(
    conn: &mut Conn,
    id: u64,
    base: u64,
    name: Option<&str>,
    description: Option<&str>,
    members: Option<&[(u64, bool)]>,
    delete: bool,
    at: &str,
    user: Option<u64>,
) -> Result<Result<u64, Stale>, String> {
    write(conn, |conn| {
        let Some(mut t) = team(conn, id)? else {
            return Err(format!("team {id} not found"));
        };
        if base != t.version {
            return Ok(Err(Stale(log_after(conn, "team", id, base)?)));
        }
        let before = team_state(&t);
        if let Some(n) = name {
            t.name = n.into();
        }
        if let Some(d) = description {
            t.description = d.into();
        }
        if let Some(m) = members {
            t.members = m.to_vec();
            t.members.sort();
            t.members.dedup_by_key(|x| x.0);
        }
        if delete {
            t.deleted = true;
        }
        let mut after = team_state(&t);
        let changed: Vec<&str> = ["name", "description", "members", "deleted"]
            .into_iter()
            .filter(|k| before[*k] != after[*k])
            .collect();
        if changed.is_empty() {
            return Ok(Ok(t.version));
        }
        let version = t.version + 1;
        conn.exec(
            "UPDATE \"_quadra_teams\" SET \"name\" = ?text, \"description\" = ?text, \"version\" = ?bigint, \"updated_at\" = ?text, \"updated_by\" = ?bigint, \"deleted_at\" = ?text, \"deleted_by\" = ?bigint WHERE \"id\" = ?bigint",
            &[
                Sql::Text(t.name.clone()),
                Sql::Text(t.description.clone()),
                Sql::Int(version as i64),
                Sql::Text(at.into()),
                opt_int(user),
                if t.deleted { Sql::Text(at.into()) } else { Sql::Null },
                if t.deleted { opt_int(user) } else { Sql::Null },
                Sql::Int(id as i64),
            ],
        )?;
        if members.is_some() {
            set_members(conn, id, &t.members)?;
        }
        after["changed"] = json!(changed);
        log(
            conn,
            "team",
            id,
            version,
            at,
            user,
            if delete { "delete" } else { "update" },
            &after,
        )?;
        Ok(Ok(version))
    })
}

// ---- deploys --------------------------------------------------------------------------------

/// The field renames of migrations, in order: (seq, entity, old name, new name).
pub fn renames(conn: &mut Conn) -> Result<Vec<(u64, String, String, String)>, String> {
    Ok(conn
        .query("SELECT CAST(\"seq\" AS TEXT), \"entity\", \"old_name\", \"new_name\" FROM \"_quadra_renames\" ORDER BY \"_quadra_renames\".\"seq\"", &[])?
        .iter()
        .map(|r| (num(&r[0]), text(&r[1]), text(&r[2]), text(&r[3])))
        .collect())
}

/// The views to check against the model `checked` (those checked against another model, or
/// never), not deleted.
pub fn unchecked(conn: &mut Conn, checked: &str) -> Result<Vec<ViewRow>, String> {
    Ok(conn
        .query(&format!("SELECT {VIEW_COLS} FROM \"_quadra_views\" WHERE \"deleted_at\" IS NULL AND (\"checked\" IS NULL OR \"checked\" <> ?text) ORDER BY \"_quadra_views\".\"id\""), &[Sql::Text(checked.into())])?
        .iter()
        .map(|r| view_of(r))
        .collect())
}

// ---- interface files ---------------------------------------------------------------------------

/// An app or a saved view of an interface file of the project, to install.
#[derive(Clone, Debug)]
pub struct FileItem {
    /// its identity: `sales/cockpit` (an app), `sales/orders#To ship` (a view)
    pub path: String,
    /// the folders of an app: the directories of its file (`["sales"]`)
    pub dirs: Vec<String>,
    /// `{kind, entity, type, name, description, config, shares}` (shares: roles and everyone)
    pub def: J,
}

/// What an install did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Installed {
    pub created: usize,
    pub changed: usize,
    pub removed: usize,
}

/// The shares an interface file gives (roles, everyone).
fn file_shares(def: &J) -> Vec<Share> {
    let mut out: Vec<Share> = def["shares"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let edit = s["edit"].as_bool().unwrap_or(false);
            match s["type"].as_str() {
                Some("role") => s["name"].as_str().map(|n| Share {
                    target: format!("role:{n}"),
                    edit,
                }),
                Some("everyone") => Some(Share {
                    target: "everyone".into(),
                    edit,
                }),
                _ => None,
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The file of an item (`sales/orders#To ship` → `sales/orders`).
pub fn file_of(path: &str) -> &str {
    path.split('#').next().unwrap_or(path)
}

/// Install the apps and views of the project's interface files, in one transaction: a new
/// one is created (`origin = file`, owned by nobody: admins administer it), one that changed
/// gets a new version (`deploy: interfaces/<file>.qdi`), one no longer in the files is
/// deleted (its history stays). The folders of apps are the directories of their files: an
/// existing folder of that name in that place is used, else one is made. The shares with
/// users and teams added in the client stay; those with roles and everyone are the file's.
pub fn install_files(
    conn: &mut Conn,
    items: &[FileItem],
    at: &str,
    checked: Option<&str>,
) -> Result<Installed, String> {
    write(conn, |conn| {
        let mut done = Installed::default();
        let all = views(conn, None, None)?;
        let mut folders: Vec<ViewRow> =
            all.iter().filter(|v| v.kind == "folder").cloned().collect();
        let mut from_files: HashMap<String, ViewRow> = all
            .iter()
            .filter(|v| v.origin.as_deref() == Some("file") && v.kind != "folder")
            .filter_map(|v| Some((v.path.clone()?, v.clone())))
            .collect();
        let mut used_folders: Vec<u64> = Vec::new();
        let empty = ViewRow {
            id: 0,
            kind: String::new(),
            entity: None,
            vtype: String::new(),
            name: String::new(),
            description: String::new(),
            config: json!({}),
            owner: 0,
            version: 1,
            created_at: String::new(),
            created_by: None,
            updated_at: String::new(),
            updated_by: None,
            deleted: false,
            broken: None,
            checked: checked.map(str::to_string),
            renames_seq: 0,
            parent: None,
            origin: Some("file".into()),
            path: None,
        };
        for it in items {
            let d = &it.def;
            let kind = d["kind"].as_str().unwrap_or("view").to_string();
            // the folders of an app
            let mut parent: Option<u64> = None;
            if kind == "app" {
                let mut chain = String::new();
                for dir in &it.dirs {
                    chain = if chain.is_empty() {
                        dir.clone()
                    } else {
                        format!("{chain}/{dir}")
                    };
                    let lower = dir.to_lowercase();
                    let found = folders
                        .iter()
                        .find(|f| f.parent == parent && f.name.to_lowercase() == lower)
                        .map(|f| f.id);
                    let id = match found {
                        Some(id) => id,
                        None => {
                            let row = ViewRow {
                                kind: "folder".into(),
                                vtype: "folder".into(),
                                name: dir.clone(),
                                parent,
                                path: Some(chain.clone()),
                                ..empty.clone()
                            };
                            let id = insert_view(
                                conn,
                                &row,
                                &[],
                                at,
                                None,
                                &format!("deploy: interfaces/{chain}"),
                            )?;
                            folders.push(ViewRow { id, ..row });
                            id
                        }
                    };
                    used_folders.push(id);
                    parent = Some(id);
                }
            }
            let shares = file_shares(d);
            let config = if d["config"].is_object() {
                d["config"].clone()
            } else {
                json!({})
            };
            let action = format!("deploy: interfaces/{}.qdi", file_of(&it.path));
            match from_files.remove(&it.path) {
                Some(row) if row.kind == kind && row.entity.as_deref() == d["entity"].as_str() => {
                    let kept: Vec<Share> = shares_of(conn, row.id)?
                        .into_iter()
                        .filter(|s| s.target.starts_with("user:") || s.target.starts_with("team:"))
                        .chain(shares.iter().cloned())
                        .collect();
                    let mut kept = kept;
                    kept.sort();
                    kept.dedup();
                    let change = ViewChange {
                        name: d["name"].as_str().map(str::to_string),
                        description: Some(d["description"].as_str().unwrap_or("").to_string()),
                        vtype: d["type"].as_str().map(str::to_string),
                        config: Some(config),
                        shares: Some(kept),
                        parent: Some(parent),
                        broken: Some(None),
                        checked: checked.map(str::to_string),
                        ..Default::default()
                    };
                    if let Ok(v) = change_view(conn, row.id, None, &change, at, None, &action)? {
                        if v != row.version {
                            done.changed += 1;
                        }
                    }
                }
                other => {
                    if let Some(old) = other {
                        let del = ViewChange {
                            delete: true,
                            ..Default::default()
                        };
                        let _ = change_view(conn, old.id, None, &del, at, None, &action)?;
                    }
                    let row = ViewRow {
                        kind: kind.clone(),
                        entity: d["entity"].as_str().map(str::to_string),
                        vtype: d["type"]
                            .as_str()
                            .unwrap_or(if kind == "app" { "app" } else { "grid" })
                            .to_string(),
                        name: d["name"].as_str().unwrap_or("").to_string(),
                        description: d["description"].as_str().unwrap_or("").to_string(),
                        config,
                        parent,
                        path: Some(it.path.clone()),
                        ..empty.clone()
                    };
                    insert_view(conn, &row, &shares, at, None, &action)?;
                    done.created += 1;
                }
            }
        }
        // what is no longer in the files
        for (path, row) in from_files {
            let del = ViewChange {
                delete: true,
                ..Default::default()
            };
            change_view(
                conn,
                row.id,
                None,
                &del,
                at,
                None,
                &format!("deploy: no longer in interfaces/{}.qdi", file_of(&path)),
            )?
            .ok();
            done.removed += 1;
        }
        // the folders made for files that hold nothing any more
        let left = views(conn, None, None)?;
        for f in folders
            .iter()
            .filter(|f| f.origin.as_deref() == Some("file") && !used_folders.contains(&f.id))
        {
            if !left.iter().any(|v| v.parent == Some(f.id)) {
                let del = ViewChange {
                    delete: true,
                    ..Default::default()
                };
                change_view(
                    conn,
                    f.id,
                    None,
                    &del,
                    at,
                    None,
                    "deploy: no longer a directory of interfaces/",
                )?
                .ok();
            }
        }
        Ok(done)
    })
}

fn shares_of(conn: &mut Conn, id: u64) -> Result<Vec<Share>, String> {
    Ok(shares(conn, Some(&[id]))?.remove(&id).unwrap_or_default())
}

/// The rows installed from one interface file (`sales/cockpit`): its app and its views.
pub fn file_rows(conn: &mut Conn, file: &str) -> Result<Vec<ViewRow>, String> {
    Ok(views(conn, None, None)?
        .into_iter()
        .filter(|v| {
            v.origin.as_deref() == Some("file")
                && v.kind != "folder"
                && v.path.as_deref().is_some_and(|p| file_of(p) == file)
        })
        .collect())
}

/// The rows of an interface file moved to another file (`sales/cockpit` → `archive/cockpit`).
pub fn move_file(conn: &mut Conn, from: &str, to: &str) -> Result<(), String> {
    for r in file_rows(conn, from)? {
        let p = r.path.unwrap_or_default();
        let new = format!("{to}{}", &p[from.len()..]);
        conn.exec(
            "UPDATE \"_quadra_views\" SET \"path\" = ?text WHERE \"id\" = ?bigint",
            &[Sql::Text(new), Sql::Int(r.id as i64)],
        )?;
    }
    Ok(())
}
