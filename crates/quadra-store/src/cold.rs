//! Hot and cold (phase 19e): the closed records of an entity with `archive …` moved out of the
//! database in use into volumes, read on demand.
//!
//! - **A volume** is a SQLite file beside the database (`<database>.cold/<name>.db`, and a copy
//!   in the project's store of files, `cold/<name>.db`) holding, for the records moved, their
//!   rows of every table that has them: the entity's table, its join and per-company tables,
//!   `_quadra_records`, `_quadra_history`. It never changes; a commit of the chain names it
//!   with its SHA-256 (`["c", name, sha256, records]`).
//! - **`_quadra_cold`** says which record is in which volume; `_quadra_volumes` lists them
//!   (a volume merged into another one, or whose records all came back, is `dropped`).
//! - **Reading** does not change: on a connection that reads (`DbSource`), a temporary view
//!   with the name of each of those tables joins the rows in use and those of the records
//!   still cold (`shadow`); SQLite takes the temporary view before the table, so the queries,
//!   the loading of records, `find` and the checks of `unique` see them all. A connection
//!   that writes has no views (`unshadow`).
//! - **Writing** a cold record brings it back first (`thaw_statements`): its rows written again
//!   into the database in use, its line of `_quadra_cold` gone; the volume keeps its copy,
//!   which no view shows any more.
//! - **A deploy** that changes the schema brings every cold record back first (`thaw_all`).

use crate::conn::{Conn, Dialect, Sql};
use crate::schema::{Schema, Source};
use std::path::{Path, PathBuf};

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE \"_quadra_cold\" (\"id\" BIGINT PRIMARY KEY, \"entity\" TEXT NOT NULL, \"volume\" TEXT NOT NULL)",
    // (`max_seq`: the highest number of a row of the history it took: the next ones are higher)
    "CREATE TABLE \"_quadra_volumes\" (\"name\" TEXT PRIMARY KEY, \"sha256\" TEXT NOT NULL, \"tx\" BIGINT NOT NULL, \"records\" BIGINT NOT NULL, \"max_seq\" BIGINT NOT NULL DEFAULT 0, \"dropped_tx\" BIGINT)",
];

/// The tables that hold rows of the records of `ent`, with the column naming the record.
pub fn tables_of(schema: &Schema, ent: &str) -> Vec<(String, &'static str)> {
    // (in the order rows can be deleted: what refers to a row before it)
    let mut out: Vec<(String, &'static str)> = vec![
        ("_quadra_history".into(), "record_id"),
        ("_quadra_postings".into(), "record_id"),
    ];
    let mut rows: Vec<(String, &'static str)> = schema
        .tables
        .iter()
        .filter_map(|t| match &t.source {
            Source::Entity(e) if e == ent => Some((t.name.clone(), "id")),
            Source::Join(e, _) | Source::PerCompany(e, _) if e == ent => {
                Some((t.name.clone(), "owner_id"))
            }
            Source::Trait(_) => Some((t.name.clone(), "id")),
            _ => None,
        })
        .collect();
    // join and per-company tables before the entity's own
    rows.sort_by_key(|(_, k)| *k != "owner_id");
    out.extend(rows);
    out.push(("_quadra_records".into(), "id"));
    out
}

/// The file of the main database of a connection (SQLite), and the folder of its volumes.
pub fn folder(conn: &mut Conn) -> Result<Option<PathBuf>, String> {
    if conn.dialect() != Dialect::Sqlite {
        return Ok(None);
    }
    let r = conn.query(
        "SELECT \"file\" FROM pragma_database_list WHERE \"name\" = 'main'",
        &[],
    )?;
    let Some(file) = r.into_iter().next().and_then(|r| r[0].clone()) else {
        return Ok(None);
    };
    if file.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(format!("{file}.cold"))))
}

/// The volumes in use: (name, its local file).
pub fn volumes(conn: &mut Conn) -> Result<Vec<(String, PathBuf)>, String> {
    if !conn.table_exists("_quadra_volumes")? {
        return Ok(Vec::new());
    }
    let Some(dir) = folder(conn)? else {
        return Ok(Vec::new());
    };
    Ok(conn
        .query(
            "SELECT \"name\" FROM \"main\".\"_quadra_volumes\" WHERE \"dropped_tx\" IS NULL ORDER BY \"name\"",
            &[],
        )?
        .into_iter()
        .filter_map(|r| r[0].clone())
        .map(|n| {
            let f = dir.join(format!("{n}.db"));
            (n, f)
        })
        .collect())
}

fn alias(name: &str) -> String {
    format!(
        "cold_{}",
        name.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    )
}

/// The tables of the volumes in use, read through views with their names (see the module).
/// Nothing when there is no volume. Idempotent.
pub fn shadow(conn: &mut Conn) -> Result<(), String> {
    let vols = volumes(conn)?;
    // the same volumes as the views of this connection: nothing to do
    let names: Vec<String> = vols.iter().map(|(n, _)| n.clone()).collect();
    let now = conn
        .query("SELECT \"v\" FROM temp.\"_quadra_shadowed\"", &[])
        .ok()
        .and_then(|r| r.into_iter().next())
        .and_then(|r| r[0].clone());
    if now.as_deref() == Some(names.join(",").as_str()) || (now.is_none() && vols.is_empty()) {
        return Ok(());
    }
    unshadow(conn)?;
    if vols.is_empty() {
        return Ok(());
    }
    let mut tables: std::collections::BTreeMap<String, &'static str> = Default::default();
    for (name, file) in &vols {
        if !file.exists() {
            return Err(format!(
                "the volume {name} of the cold records is not at {} (`quadra db cold --fetch` brings it from the store of files)",
                file.display()
            ));
        }
        let a = alias(name);
        conn.batch(&format!(
            "ATTACH DATABASE '{}' AS \"{a}\"",
            file.display().to_string().replace('\'', "''")
        ))?;
        for r in conn.query(
            &format!("SELECT \"name\" FROM \"{a}\".sqlite_master WHERE \"type\" = 'table'"),
            &[],
        )? {
            let Some(t) = r[0].clone() else { continue };
            let key = match t.as_str() {
                "_quadra_history" => "record_id",
                _ => {
                    let cols = conn.query(
                        &format!(
                            "SELECT \"name\" FROM pragma_table_info('{}', '{a}')",
                            t.replace('\'', "''")
                        ),
                        &[],
                    )?;
                    if cols.iter().any(|c| c[0].as_deref() == Some("owner_id")) {
                        "owner_id"
                    } else {
                        "id"
                    }
                }
            };
            tables.insert(t, key);
        }
    }
    for (t, key) in &tables {
        let cols: Vec<String> = conn
            .query(
                &format!(
                    "SELECT \"name\" FROM pragma_table_info('{}', 'main') ORDER BY \"cid\"",
                    t.replace('\'', "''")
                ),
                &[],
            )?
            .into_iter()
            .filter_map(|r| r[0].clone())
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect();
        if cols.is_empty() {
            continue;
        }
        let list = cols.join(", ");
        let mut sql = format!("CREATE TEMP VIEW \"{t}\" AS SELECT {list} FROM \"main\".\"{t}\"");
        for (name, _) in &vols {
            let a = alias(name);
            let has = conn.query(
                &format!("SELECT 1 FROM \"{a}\".sqlite_master WHERE \"type\" = 'table' AND \"name\" = ?text"),
                &[Sql::Text(t.clone())],
            )?;
            if has.is_empty() {
                continue;
            }
            sql.push_str(&format!(
                " UNION ALL SELECT {list} FROM \"{a}\".\"{t}\" WHERE \"{key}\" IN (SELECT \"id\" FROM \"main\".\"_quadra_cold\" WHERE \"volume\" = '{}')",
                name.replace('\'', "''")
            ));
        }
        conn.batch(&sql)?;
    }
    conn.batch("CREATE TEMP TABLE IF NOT EXISTS \"_quadra_shadowed\" (\"v\" TEXT)")?;
    conn.exec(
        "INSERT INTO temp.\"_quadra_shadowed\" (\"v\") VALUES (?text)",
        &[Sql::Text(names.join(","))],
    )?;
    Ok(())
}

/// The views of `shadow` dropped, the volumes detached: the connection reads and writes the
/// database in use alone.
pub fn unshadow(conn: &mut Conn) -> Result<(), String> {
    if conn.dialect() != Dialect::Sqlite {
        return Ok(());
    }
    let shadowed = conn
        .query(
            "SELECT 1 FROM temp.sqlite_master WHERE \"name\" = '_quadra_shadowed'",
            &[],
        )
        .map(|r| !r.is_empty())
        .unwrap_or(false);
    if !shadowed {
        return Ok(());
    }
    for r in conn.query(
        "SELECT \"name\" FROM temp.sqlite_master WHERE \"type\" = 'view'",
        &[],
    )? {
        if let Some(v) = &r[0] {
            conn.batch(&format!("DROP VIEW temp.\"{}\"", v.replace('"', "\"\"")))?;
        }
    }
    for r in conn.query("SELECT \"name\" FROM pragma_database_list", &[])? {
        if let Some(n) = &r[0] {
            if n.starts_with("cold_") {
                conn.batch(&format!("DETACH DATABASE \"{n}\""))?;
            }
        }
    }
    conn.batch("DROP TABLE temp.\"_quadra_shadowed\"")?;
    Ok(())
}

/// The rows of `ids` (cold, in `volume`) as statements writing them again into the database in
/// use, with their values as literals (a copy of the database rebuilt from the log replays
/// them without the volume), and their lines of `_quadra_cold` deleted.
pub fn thaw_statements(
    volume_file: &Path,
    tables: &[(String, &'static str)],
    ids: &[u64],
) -> Result<Vec<String>, String> {
    let mut v = Conn::open(&format!("sqlite:{}", volume_file.display()))?;
    let list: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    let list = list.join(", ");
    let mut out = Vec::new();
    for (t, key) in tables {
        let cols: Vec<String> = v
            .query(
                &format!(
                    "SELECT \"name\" FROM pragma_table_info('{}') ORDER BY \"cid\"",
                    t.replace('\'', "''")
                ),
                &[],
            )?
            .into_iter()
            .filter_map(|r| r[0].clone())
            .collect();
        if cols.is_empty() {
            continue;
        }
        let quoted: Vec<String> = cols.iter().map(|c| format!("quote(\"{c}\")")).collect();
        let names: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
        for r in v.query(
            &format!(
                "SELECT {} FROM \"{t}\" WHERE \"{key}\" IN ({list})",
                quoted.join(", ")
            ),
            &[],
        )? {
            let vals: Vec<String> = r
                .into_iter()
                .map(|x| x.unwrap_or_else(|| "NULL".into()))
                .collect();
            out.push(format!(
                "INSERT INTO \"{t}\" ({}) VALUES ({})",
                names.join(", "),
                vals.join(", ")
            ));
        }
    }
    out.push(format!(
        "DELETE FROM \"_quadra_cold\" WHERE \"id\" IN ({list})"
    ));
    Ok(out)
}

/// The entry of a volume in its commit's hash.
pub(crate) fn entry(name: &str, sha256: &str, records: u64) -> serde_json::Value {
    serde_json::json!(["c", name, sha256, records])
}

/// The volumes as entries of their commits' hashes: (tx, entry).
pub(crate) fn entries(conn: &mut Conn) -> Result<Vec<(i64, String)>, String> {
    if !conn.table_exists("_quadra_volumes")? {
        return Ok(Vec::new());
    }
    Ok(conn
        .query(
            "SELECT CAST(\"tx\" AS TEXT), \"name\", \"sha256\", CAST(\"records\" AS TEXT) FROM \"main\".\"_quadra_volumes\" ORDER BY \"tx\"",
            &[],
        )?
        .into_iter()
        .map(|r| {
            let n = |i: usize| r[i].clone().unwrap_or_default();
            (
                n(0).parse().unwrap_or(0),
                entry(&n(1), &n(2), n(3).parse().unwrap_or(0)).to_string(),
            )
        })
        .collect())
}

/// Before a deploy: the cold records of the entities whose tables, formulas, defaults or traits
/// change, back in the database in use (the deploy rewrites what it changes there, the volumes
/// never change). The others stay cold. Returns how many came back.
pub fn thaw_for_deploy(
    url: &str,
    model: &quadra_ir::Model,
    report: &quadra_types::Report,
    all: bool,
) -> Result<usize, String> {
    let mut db = crate::Database::open(url, model, report)?;
    if !db.conn.table_exists("_quadra_cold")?
        || db
            .conn
            .query("SELECT 1 FROM \"_quadra_cold\" LIMIT 1", &[])?
            .is_empty()
    {
        return Ok(0);
    }
    let old = crate::deploy::Catalog::read(&mut db)?;
    let new = crate::deploy::Catalog::of(model, &db.schema, old.as_ref());
    let changed: std::collections::BTreeSet<String> = match &old {
        // no catalog, or a deploy that rebuilds every table: everything comes back
        _ if all => model.entities.iter().map(|e| e.name.clone()).collect(),
        None => model.entities.iter().map(|e| e.name.clone()).collect(),
        Some(old) => model
            .entities
            .iter()
            .filter(|e| {
                let mine = |c: &crate::deploy::Catalog| -> String {
                    let tables: Vec<String> = c
                        .tables
                        .iter()
                        .filter(|t| match &t.source {
                            Source::Entity(x) | Source::Join(x, _) | Source::PerCompany(x, _) => {
                                *x == e.name
                            }
                            _ => false,
                        })
                        .map(|t| {
                            format!(
                                "{}:{:?}",
                                t.name,
                                t.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
                            )
                        })
                        .collect();
                    let pre = format!("{}.", e.name);
                    let formulas: Vec<String> = c
                        .formulas
                        .iter()
                        .filter(|(k, _)| k.starts_with(&pre))
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    let defaults: Vec<&String> =
                        c.defaults.iter().filter(|k| k.starts_with(&pre)).collect();
                    let traits: Vec<&String> = c
                        .traits
                        .iter()
                        .filter(|(_, v)| v.contains(&e.name))
                        .map(|(k, _)| k)
                        .collect();
                    format!("{tables:?}{formulas:?}{defaults:?}{traits:?}")
                };
                mine(old) != mine(&new)
            })
            .map(|e| e.name.clone())
            .collect(),
    };
    if changed.is_empty() {
        return Ok(0);
    }
    let schema = db.schema.clone();
    let vols = volumes(&mut db.conn)?;
    drop(db);
    let mut c = Conn::open(url)?;
    let mut n = 0;
    for (name, file) in vols {
        c.batch(&format!(
            "ATTACH DATABASE '{}' AS \"thawing\"",
            file.display().to_string().replace('\'', "''")
        ))?;
        let r = (|| -> Result<usize, String> {
            c.batch("BEGIN IMMEDIATE")?;
            let mut moved = 0;
            for ent in &changed {
                let ids: Vec<String> = c
                    .query(
                        "SELECT CAST(\"id\" AS TEXT) FROM \"main\".\"_quadra_cold\" WHERE \"volume\" = ?text AND \"entity\" = ?text",
                        &[Sql::Text(name.clone()), Sql::Text(ent.clone())],
                    )?
                    .into_iter()
                    .filter_map(|r| r[0].clone())
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                for chunk in ids.chunks(500) {
                    let list = chunk.join(", ");
                    for (t, key) in tables_of(&schema, ent) {
                        let has = c.query("SELECT 1 FROM \"thawing\".sqlite_master WHERE \"type\" = 'table' AND \"name\" = ?text", &[Sql::Text(t.clone())])?;
                        if has.is_empty() {
                            continue;
                        }
                        c.batch(&format!("INSERT INTO \"main\".\"{t}\" SELECT * FROM \"thawing\".\"{t}\" WHERE \"{key}\" IN ({list})"))?;
                    }
                    c.batch(&format!(
                        "DELETE FROM \"main\".\"_quadra_cold\" WHERE \"id\" IN ({list})"
                    ))?;
                }
                moved += ids.len();
            }
            c.batch("COMMIT")?;
            Ok(moved)
        })();
        if r.is_err() {
            let _ = c.batch("ROLLBACK");
        }
        let _ = c.batch("DETACH DATABASE \"thawing\"");
        n += r?;
    }
    Ok(n)
}

/// The volumes in use whose file is not beside the database (a copy rebuilt from the log, a new
/// machine): each fetched with `get` (from the store of files, `cold/<name>.db`) and checked
/// against its SHA-256. Returns how many came.
pub fn fetch(
    url: &str,
    get: &mut dyn FnMut(&str) -> Result<Option<Vec<u8>>, String>,
) -> Result<usize, String> {
    use sha2::Digest;
    let mut c = Conn::open(url)?;
    let Some(dir) = folder(&mut c)? else {
        return Ok(0);
    };
    let mut n = 0;
    for (name, file) in volumes(&mut c)? {
        if file.exists() {
            continue;
        }
        let bytes = get(&format!("cold/{name}.db"))?.ok_or(format!(
            "the volume {name} is not in the store of files (cold/{name}.db)"
        ))?;
        let sha: String = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let want = c
            .query(
                "SELECT \"sha256\" FROM \"main\".\"_quadra_volumes\" WHERE \"name\" = ?text",
                &[Sql::Text(name.clone())],
            )?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone())
            .unwrap_or_default();
        if sha != want {
            return Err(format!("the volume {name} of the store of files is not the one its commit names (sha256 {sha}, not {want})"));
        }
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

/// The volumes in use holding records no longer cold in them (they came back: written,
/// erased, deleted): to be written again without them.
pub fn dirty(conn: &mut Conn) -> Result<Vec<String>, String> {
    if !conn.table_exists("_quadra_volumes")? {
        return Ok(Vec::new());
    }
    Ok(conn
        .query(
            "SELECT v.\"name\" FROM \"main\".\"_quadra_volumes\" v WHERE v.\"dropped_tx\" IS NULL AND v.\"records\" > (SELECT COUNT(*) FROM \"main\".\"_quadra_cold\" c WHERE c.\"volume\" = v.\"name\")",
            &[],
        )?
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect())
}
