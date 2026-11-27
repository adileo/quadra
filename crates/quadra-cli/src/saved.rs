//! `quadra db views export <file> | import <file>`: the saved views, apps, folders and teams
//! of a database as one JSON file, with users by login and teams by name, so that they move to
//! another database (another installation, a copy for tests) whatever the ids there. Each item
//! has a `key` (its id where it was exported) and the `parent` folder's key: the tree moves
//! with them.

use quadra_store::conn::Conn;
use quadra_store::views::{self as store, Share, ViewRow};
use serde_json::{json, Value as J};
use std::collections::HashMap;

/// The table of `User` in the database's schema.
fn users_table(schema: &quadra_store::schema::Schema) -> Result<String, String> {
    schema
        .tables
        .iter()
        .find(|t| matches!(&t.source, quadra_store::schema::Source::Entity(e) if e == "User"))
        .map(|t| t.name.clone())
        .ok_or_else(|| "the model has no User entity".to_string())
}

/// Every user: id → login.
fn logins(conn: &mut Conn, table: &str) -> Result<HashMap<u64, String>, String> {
    Ok(conn
        .query(
            &format!("SELECT CAST(\"id\" AS TEXT), \"login\" FROM \"{table}\""),
            &[],
        )?
        .into_iter()
        .filter_map(|r| Some((r[0].as_deref()?.parse().ok()?, r[1].clone()?)))
        .collect())
}

pub fn export(url: &str, schema: &quadra_store::schema::Schema) -> Result<J, String> {
    let mut conn = Conn::open(url)?;
    let table = users_table(schema)?;
    let logins = logins(&mut conn, &table)?;
    let login = |id: u64| logins.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
    let teams = store::teams(&mut conn)?;
    let team_name: HashMap<u64, String> = teams.iter().map(|t| (t.id, t.name.clone())).collect();
    // the apps and views of interface files are code: they are in the project, not exported
    let views: Vec<ViewRow> = store::views(&mut conn, None, None)?
        .into_iter()
        .filter(|v| v.origin.as_deref() != Some("file"))
        .collect();
    let shares = store::shares(&mut conn, None)?;
    let out_teams: Vec<J> = teams
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "members": t.members.iter().map(|(u, o)| json!({ "login": login(*u), "owner": o })).collect::<Vec<_>>() }))
        .collect();
    let out_views: Vec<J> = views
        .iter()
        .map(|v| {
            let sh: Vec<J> = shares
                .get(&v.id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter_map(|s| match s.target.split_once(':') {
                    Some(("user", id)) => Some(json!({ "type": "user", "login": login(id.parse().ok()?), "edit": s.edit })),
                    // a share with a deleted team is not exported
                    Some(("team", id)) => Some(json!({ "type": "team", "name": team_name.get(&id.parse().ok()?)?, "edit": s.edit })),
                    Some(("role", r)) => Some(json!({ "type": "role", "name": r, "edit": s.edit })),
                    _ => Some(json!({ "type": "everyone", "edit": s.edit })),
                })
                .collect();
            json!({
                "key": v.id, "parent": v.parent,
                "kind": v.kind, "entity": v.entity, "type": v.vtype, "name": v.name, "description": v.description,
                "owner": login(v.owner), "config": v.config, "shares": sh,
            })
        })
        .collect();
    // 2: with folders (`key`, `parent`)
    Ok(json!({ "quadra_views": 2, "teams": out_teams, "views": out_views }))
}

/// Load an export: teams that do not exist yet (by name) are created, members added to the
/// others; folders that do not exist yet (by name, in the same folder) are created, parents
/// first; every view and app is created again for its owner, with its shares, in its folder
/// (at the top when its folder is not in the file). One transaction: nothing is written if a
/// login or a team is unknown. The views are checked against the model when the server starts.
/// Returns (teams created, folders created, views and apps created).
pub fn import(
    url: &str,
    schema: &quadra_store::schema::Schema,
    doc: &J,
    at: &str,
) -> Result<(usize, usize, usize), String> {
    if !matches!(doc["quadra_views"].as_u64(), Some(1 | 2)) {
        return Err("not an export of `quadra db views export`".into());
    }
    let mut conn = Conn::open(url)?;
    let table = users_table(schema)?;
    let by_login: HashMap<String, u64> = logins(&mut conn, &table)?
        .into_iter()
        .map(|(id, l)| (l, id))
        .collect();
    let user = |l: &J| -> Result<u64, String> {
        let l = l.as_str().unwrap_or("");
        by_login
            .get(l)
            .copied()
            .ok_or_else(|| format!("no user `{l}` in this database"))
    };
    store::write(&mut conn, |conn| {
        let mut teams: HashMap<String, u64> = store::teams(conn)?
            .into_iter()
            .map(|t| (t.name, t.id))
            .collect();
        let mut created_teams = 0;
        for t in doc["teams"].as_array().into_iter().flatten() {
            let name = t["name"]
                .as_str()
                .ok_or("a team without a name")?
                .to_string();
            let members: Vec<(u64, bool)> = t["members"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| Ok((user(&m["login"])?, m["owner"].as_bool().unwrap_or(false))))
                .collect::<Result<_, String>>()?;
            if teams.contains_key(&name) {
                continue;
            }
            let id = store::insert_team(
                conn,
                &name,
                t["description"].as_str().unwrap_or(""),
                &members,
                at,
                None,
                "import",
            )?;
            teams.insert(name, id);
            created_teams += 1;
        }
        let renames_seq = store::renames(conn)?.last().map(|r| r.0).unwrap_or(0);
        let items: Vec<&J> = doc["views"].as_array().into_iter().flatten().collect();
        // folders, parents first: key in the file → id here
        let mut ids: HashMap<u64, u64> = HashMap::new();
        let mut here = store::views(conn, Some("folder"), None)?;
        let mut pending: Vec<&J> = items
            .iter()
            .copied()
            .filter(|v| v["kind"] == json!("folder"))
            .collect();
        let mut created_folders = 0;
        while !pending.is_empty() {
            // those whose folder is placed (or not in the file: at the top)
            let (ready, wait): (Vec<&J>, Vec<&J>) = pending.iter().partition(|v| {
                v["parent"].as_u64().map_or(true, |p| {
                    ids.contains_key(&p)
                        || !items
                            .iter()
                            .any(|x| x["kind"] == json!("folder") && x["key"].as_u64() == Some(p))
                })
            });
            // a loop in the file: the rest at the top
            let (ready, wait) = if ready.is_empty() {
                (wait, Vec::new())
            } else {
                (ready, wait)
            };
            for f in ready {
                let parent = f["parent"].as_u64().and_then(|p| ids.get(&p).copied());
                let name: String = f["name"]
                    .as_str()
                    .unwrap_or("Imported folder")
                    .chars()
                    .take(100)
                    .collect();
                let id = match here
                    .iter()
                    .find(|x| x.parent == parent && x.name.to_lowercase() == name.to_lowercase())
                {
                    Some(x) => x.id,
                    None => {
                        let row = ViewRow {
                            kind: "folder".into(),
                            entity: None,
                            vtype: "folder".into(),
                            name,
                            config: json!({}),
                            parent,
                            ..row_of(f, user(&f["owner"])?, renames_seq)
                        };
                        let id = store::insert_view(
                            conn,
                            &row,
                            &shares_of(f, &user, &teams)?,
                            at,
                            None,
                            "import",
                        )?;
                        here.push(ViewRow { id, ..row });
                        created_folders += 1;
                        id
                    }
                };
                if let Some(k) = f["key"].as_u64() {
                    ids.insert(k, id);
                }
            }
            pending = wait;
        }
        let mut created = 0;
        for v in items.iter().filter(|v| v["kind"] != json!("folder")) {
            let shares = shares_of(v, &user, &teams)?;
            for (k, e) in [("where", &v["config"]["where"])] {
                if e.as_str().map_or(false, |x| x.contains('\n')) {
                    return Err(format!("view `{}`: its {k} is not one line", v["name"]));
                }
            }
            // views of entities are in no folder
            let parent = if v["kind"] == json!("app") {
                v["parent"].as_u64().and_then(|p| ids.get(&p).copied())
            } else {
                None
            };
            let row = ViewRow {
                parent,
                ..row_of(v, user(&v["owner"])?, renames_seq)
            };
            store::insert_view(conn, &row, &shares, at, None, "import")?;
            created += 1;
        }
        Ok((created_teams, created_folders, created))
    })
}

/// A row from an item of the file, for its owner.
fn row_of(v: &J, owner: u64, renames_seq: u64) -> ViewRow {
    ViewRow {
        id: 0,
        kind: v["kind"].as_str().unwrap_or("view").into(),
        entity: v["entity"].as_str().map(str::to_string),
        vtype: v["type"].as_str().unwrap_or("grid").into(),
        name: v["name"]
            .as_str()
            .unwrap_or("Imported view")
            .chars()
            .take(100)
            .collect(),
        description: v["description"].as_str().unwrap_or("").into(),
        config: if v["config"].is_object() {
            v["config"].clone()
        } else {
            json!({})
        },
        owner,
        version: 1,
        created_at: String::new(),
        created_by: None,
        updated_at: String::new(),
        updated_by: None,
        deleted: false,
        broken: None,
        // checked against the model by the server when it starts
        checked: None,
        renames_seq,
        parent: None,
        origin: None,
        path: None,
    }
}

/// The shares of an item of the file: users by login, teams by name.
fn shares_of(
    v: &J,
    user: &dyn Fn(&J) -> Result<u64, String>,
    teams: &HashMap<String, u64>,
) -> Result<Vec<Share>, String> {
    let mut shares = Vec::new();
    for s in v["shares"].as_array().into_iter().flatten() {
        let target = match s["type"].as_str() {
            Some("user") => format!("user:{}", user(&s["login"])?),
            Some("team") => {
                let n = s["name"].as_str().unwrap_or("");
                format!(
                    "team:{}",
                    teams.get(n).ok_or_else(|| format!("no team `{n}`"))?
                )
            }
            Some("role") => format!("role:{}", s["name"].as_str().unwrap_or("")),
            _ => "everyone".into(),
        };
        shares.push(Share {
            target,
            edit: s["edit"].as_bool().unwrap_or(false),
        });
    }
    Ok(shares)
}
