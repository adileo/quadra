//! The database: system tables (truth + signed commit log) and projections (one table per
//! entity, readable by anyone with SQL).
//!
//! Truth: `_quadra_records`, `_quadra_history` (every write of every field),
//! `_quadra_ledger_rows`, `_quadra_postings`, `_quadra_counters`, `_quadra_meta`.
//! Each transaction of the engine is one database transaction, recorded in
//! `_quadra_commits` with `hash = sha256(prev_hash, transaction)`.
//! Projections are rebuilt from the truth on migration and checked by `verify`.

use crate::codec::{cause_from_json, cause_to_json, date_text, dec_text, from_json, to_json};
use crate::conn::{Conn, Dialect, Pooled, Sql};
use crate::schema::{self, ColType, Column, FieldMap, Part, Schema, Source, Table};
use crate::source::renamed;
use quadra_ir::Model;
use quadra_runtime::store::{LedgerRow, ReadSet, Record, Store};
use quadra_runtime::{Commit, CommitError, CommitSink, Conflict, V};
use quadra_types::Report;
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const SYSTEM_DDL: &[&str] = &[
    "CREATE TABLE \"_quadra_meta\" (\"key\" TEXT PRIMARY KEY, \"value\" TEXT NOT NULL)",
    "CREATE TABLE \"_quadra_commits\" (\"tx\" BIGINT PRIMARY KEY, \"at\" TEXT NOT NULL, \"user_id\" BIGINT, \"cause\" TEXT NOT NULL, \"prev_hash\" TEXT NOT NULL, \"hash\" TEXT NOT NULL)",
    "CREATE TABLE \"_quadra_records\" (\"id\" BIGINT PRIMARY KEY, \"entity\" TEXT NOT NULL, \"created_tx\" BIGINT NOT NULL, \"created_at\" TEXT NOT NULL, \"created_by\" BIGINT, \"frozen_tx\" BIGINT, \"cause\" TEXT NOT NULL, \"warnings\" TEXT NOT NULL, \"last_tx\" BIGINT NOT NULL DEFAULT 0, \"version_tx\" BIGINT NOT NULL DEFAULT 0, \"kinds\" TEXT)",
    "CREATE TABLE \"_quadra_history\" (\"seq\" BIGINT PRIMARY KEY, \"tx\" BIGINT NOT NULL, \"record_id\" BIGINT NOT NULL, \"field\" TEXT NOT NULL, \"value\" TEXT NOT NULL)",
    "CREATE TABLE \"_quadra_ledger_rows\" (\"id\" BIGINT PRIMARY KEY, \"ledger\" TEXT NOT NULL, \"tx\" BIGINT NOT NULL, \"date\" TEXT NOT NULL, \"source\" TEXT NOT NULL, \"cols\" TEXT NOT NULL)",
    "CREATE TABLE \"_quadra_postings\" (\"record_id\" BIGINT NOT NULL, \"ledger_key\" TEXT NOT NULL, \"rows\" TEXT NOT NULL, PRIMARY KEY (\"record_id\", \"ledger_key\"))",
    "CREATE TABLE \"_quadra_counters\" (\"key\" TEXT PRIMARY KEY, \"n\" BIGINT NOT NULL)",
    "CREATE TABLE \"_quadra_tokens\" (\"hash\" TEXT PRIMARY KEY, \"kind\" TEXT NOT NULL, \"user_id\" BIGINT NOT NULL, \"name\" TEXT NOT NULL, \"read_only\" BOOLEAN NOT NULL, \"csrf\" TEXT, \"created_at\" TEXT NOT NULL, \"expires_at\" TEXT, \"revoked\" BOOLEAN NOT NULL)",
    "CREATE TABLE \"_quadra_idempotency\" (\"user_id\" BIGINT NOT NULL, \"key\" TEXT NOT NULL, \"request_hash\" TEXT NOT NULL, \"response\" TEXT NOT NULL, \"tx\" BIGINT NOT NULL, \"created_at\" TEXT NOT NULL, PRIMARY KEY (\"user_id\", \"key\"))",
    "CREATE TABLE \"_quadra_applied\" (\"key\" TEXT PRIMARY KEY, \"tx\" BIGINT NOT NULL)",
    "CREATE TABLE \"_quadra_renames\" (\"seq\" BIGINT PRIMARY KEY, \"tx\" BIGINT NOT NULL, \"entity\" TEXT NOT NULL, \"old_name\" TEXT NOT NULL, \"new_name\" TEXT NOT NULL)",
    "CREATE TABLE \"_quadra_outbox\" (\"id\" BIGINT PRIMARY KEY, \"tx\" BIGINT NOT NULL, \"event\" TEXT NOT NULL, \"fields\" TEXT NOT NULL, \"created_at\" TEXT NOT NULL, \"delivered\" BOOLEAN NOT NULL, \"delivered_at\" TEXT)",
    // what users make in the web client (crate::views): saved views and boards, shared with
    // users, teams, roles or everyone; teams; each user's favourites, defaults and order; and
    // the log of every change (who, when, what), by object and version
    "CREATE TABLE \"_quadra_views\" (\"id\" BIGINT PRIMARY KEY, \"kind\" TEXT NOT NULL, \"entity\" TEXT, \"type\" TEXT NOT NULL, \"name\" TEXT NOT NULL, \"description\" TEXT NOT NULL, \"config\" TEXT NOT NULL, \"owner_id\" BIGINT NOT NULL, \"version\" BIGINT NOT NULL, \"created_at\" TEXT NOT NULL, \"created_by\" BIGINT, \"updated_at\" TEXT NOT NULL, \"updated_by\" BIGINT, \"deleted_at\" TEXT, \"deleted_by\" BIGINT, \"broken\" TEXT, \"checked\" TEXT, \"renames_seq\" BIGINT NOT NULL, \"parent_id\" BIGINT, \"origin\" TEXT, \"path\" TEXT)",
    "CREATE TABLE \"_quadra_view_shares\" (\"view_id\" BIGINT NOT NULL, \"target\" TEXT NOT NULL, \"can_edit\" BOOLEAN NOT NULL, PRIMARY KEY (\"view_id\", \"target\"))",
    "CREATE TABLE \"_quadra_view_prefs\" (\"user_id\" BIGINT NOT NULL, \"view_id\" BIGINT NOT NULL, \"favourite\" BOOLEAN NOT NULL, \"position\" BIGINT, \"is_default\" BOOLEAN NOT NULL, \"state\" TEXT, PRIMARY KEY (\"user_id\", \"view_id\"))",
    "CREATE TABLE \"_quadra_teams\" (\"id\" BIGINT PRIMARY KEY, \"name\" TEXT NOT NULL, \"description\" TEXT NOT NULL, \"version\" BIGINT NOT NULL, \"created_at\" TEXT NOT NULL, \"created_by\" BIGINT, \"updated_at\" TEXT NOT NULL, \"updated_by\" BIGINT, \"deleted_at\" TEXT, \"deleted_by\" BIGINT)",
    "CREATE TABLE \"_quadra_team_members\" (\"team_id\" BIGINT NOT NULL, \"user_id\" BIGINT NOT NULL, \"owner\" BOOLEAN NOT NULL, PRIMARY KEY (\"team_id\", \"user_id\"))",
    "CREATE TABLE \"_quadra_view_log\" (\"object\" TEXT NOT NULL, \"object_id\" BIGINT NOT NULL, \"version\" BIGINT NOT NULL, \"at\" TEXT NOT NULL, \"user_id\" BIGINT, \"action\" TEXT NOT NULL, \"detail\" TEXT NOT NULL, PRIMARY KEY (\"object\", \"object_id\", \"version\"))",
];

/// The indexes of the system tables, one per access path of the engine: a record's history
/// (loading, `history`), the writes of a transaction (conflicts, live updates), the records
/// of an entity (scans), those written or created after a transaction (the cache, conflicts),
/// the rows of a ledger.
pub const SYSTEM_INDEXES: &[(&str, &str, &[&str])] = &[
    ("qx__quadra_history_record_id_seq", "_quadra_history", &["record_id", "seq"]),
    ("qx__quadra_history_tx", "_quadra_history", &["tx"]),
    ("qx__quadra_records_entity_id", "_quadra_records", &["entity", "id"]),
    ("qx__quadra_records_last_tx", "_quadra_records", &["last_tx"]),
    ("qx__quadra_records_created_tx", "_quadra_records", &["created_tx"]),
    ("qx__quadra_ledger_rows_ledger_tx", "_quadra_ledger_rows", &["ledger", "tx"]),
];

/// The version of the system tables and their indexes: a database opened with an older one
/// is upgraded (tables and indexes added, nothing removed).
///
/// `_quadra_records`: `last_tx` is the last transaction that changed the record's row in any
/// way (a field, its metadata, a materialized formula: what a cache or a snapshot check must
/// see), `version_tx` the last one that wrote one of its fields (its history: `updated_at`,
/// the API's `version`). `kinds`: the stored values held by columns of another kind of
/// number (an integer in a `Decimal` field: `{"factor": "int"}`), so that they read back as
/// themselves (`kinds_of`).
///
/// 7: `_quadra_views.parent_id`, the folder an app or a folder is in (crate::views).
/// 8: `_quadra_views.origin` and `path`: apps, views and folders installed from the project's
/// interface files (`interfaces/**/*.qdi`), by their path there.
const SYSTEM_VERSION: &str = "8";

/// The system tables as created on a dialect (SQLite: a `BIGINT PRIMARY KEY` is the table's
/// rowid, `INTEGER PRIMARY KEY`: one B-tree instead of two).
fn system_ddl(d: Dialect) -> Vec<String> {
    SYSTEM_DDL.iter().map(|s| if d == Dialect::Sqlite { s.replacen("BIGINT PRIMARY KEY", "INTEGER PRIMARY KEY", 1) } else { s.to_string() }).collect()
}

pub(crate) fn system_indexes_sql() -> Vec<String> {
    SYSTEM_INDEXES.iter().map(|(n, t, cols)| format!("CREATE INDEX IF NOT EXISTS \"{n}\" ON \"{t}\" ({})", cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", "))).collect()
}

/// An event of the outbox (`emit`), as written with the commit that emitted it.
#[derive(Clone, Debug)]
pub struct OutboxEvent {
    pub id: u64,
    pub tx: u64,
    pub event: String,
    /// the event's fields: name → value, in the encoding of the history
    pub fields: J,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

pub struct Database {
    pub conn: Pooled,
    pub schema: Rc<Schema>,
    pub(crate) entity_tables: HashMap<String, usize>,
    pub(crate) seq: i64,
    pub(crate) prev_hash: String,
    /// writes of a commit held to be sent together (PostgreSQL: one statement of
    /// data-modifying `WITH`s, one round trip): see `wexec`, `flush`
    deferred: Option<Vec<(String, Vec<Sql>)>>,
    /// the URL it was opened with (`open`), for the helper connections of a bulk import
    pub(crate) url: Option<String>,
}

#[derive(Debug)]
pub struct Status {
    pub initialized: bool,
    pub schema_current: bool,
    pub stored_fingerprint: Option<String>,
    pub fingerprint: String,
    pub commits: i64,
    pub last_hash: Option<String>,
}

/// Formula fields materialized as columns: (entity, field).
pub fn materialized_formulas(schema: &Schema) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for t in &schema.tables {
        if let Source::Entity(e) = &t.source {
            for f in t.fields.iter().filter(|f| f.formula) {
                out.push((e.clone(), f.field.clone()));
            }
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SQLite: the triggers keeping the FTS5 table of a table (`Database::sync_search` builds the
/// table again when one is missing): before its columns are dropped or the table is copied.
pub(crate) fn drop_search_triggers(conn: &mut Conn, table: &str) -> Result<(), String> {
    let fts = crate::schema::fts_table(table);
    for k in ["ai", "ad", "au"] {
        conn.batch(&format!("DROP TRIGGER IF EXISTS {}", q(&format!("{fts}_{k}"))))?;
    }
    Ok(())
}

fn q(name: &str) -> String {
    format!("\"{name}\"")
}

fn num(s: &Option<String>) -> i64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

impl Database {
    pub fn open(url: &str, model: &Model, report: &Report) -> Result<Database, String> {
        let conn = Pooled::own(Conn::open(url)?);
        let mut db = Database::with(conn, Rc::new(schema::build(model, report)));
        db.url = Some(url.to_string());
        db.upgrade()?;
        Ok(db)
    }

    /// A database over a connection already open (an engine's pool), with the schema of the
    /// model it serves: nothing is checked or upgraded.
    pub fn with(conn: Pooled, schema: Rc<Schema>) -> Database {
        let entity_tables = schema
            .tables
            .iter()
            .enumerate()
            .filter_map(|(i, t)| match &t.source {
                Source::Entity(e) | Source::Ledger(e) => Some((e.clone(), i)),
                _ => None,
            })
            .collect();
        Database { conn, schema, entity_tables, seq: 0, prev_hash: "0".repeat(64), deferred: None, url: None }
    }

    /// Databases created by an older version get the system tables and indexes added since.
    /// One query when the database is current.
    ///
    /// A column is looked for qualified by its table (`t."col"`): SQLite reads an unknown
    /// `"col"` alone as a string, and the column would never be added.
    fn upgrade(&mut self) -> Result<(), String> {
        match self.conn.query("SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'system'", &[]) {
            Err(_) => return Ok(()), // not initialized
            Ok(r) if r.first().and_then(|r| r[0].as_deref()) == Some(SYSTEM_VERSION) => return Ok(()),
            Ok(_) => {}
        }
        if self.conn.query("SELECT t.\"last_tx\" FROM \"_quadra_records\" t WHERE 1 = 0", &[]).is_err() {
            self.conn.batch("ALTER TABLE \"_quadra_records\" ADD COLUMN \"last_tx\" BIGINT NOT NULL DEFAULT 0")?;
        }
        if self.conn.query("SELECT t.\"version_tx\" FROM \"_quadra_records\" t WHERE 1 = 0", &[]).is_err() {
            // 0: the rows written before are read from the history (their decimals were written
            // without their scale); each record's next write sets it
            self.conn.batch("ALTER TABLE \"_quadra_records\" ADD COLUMN \"version_tx\" BIGINT NOT NULL DEFAULT 0")?;
        }
        if self.conn.query("SELECT t.\"kinds\" FROM \"_quadra_records\" t WHERE 1 = 0", &[]).is_err() {
            self.conn.batch("ALTER TABLE \"_quadra_records\" ADD COLUMN \"kinds\" TEXT")?;
        }
        // the user's own state of a saved view (an app: the default values of its filters)
        if self.conn.table_exists("_quadra_view_prefs")? && self.conn.query("SELECT t.\"state\" FROM \"_quadra_view_prefs\" t WHERE 1 = 0", &[]).is_err() {
            self.conn.batch("ALTER TABLE \"_quadra_view_prefs\" ADD COLUMN \"state\" TEXT")?;
        }
        // the folder an app or a folder is in (none: at the top)
        if self.conn.table_exists("_quadra_views")? && self.conn.query("SELECT t.\"parent_id\" FROM \"_quadra_views\" t WHERE 1 = 0", &[]).is_err() {
            self.conn.batch("ALTER TABLE \"_quadra_views\" ADD COLUMN \"parent_id\" BIGINT")?;
        }
        // where an app, a view or a folder comes from: an interface file (`file`), by its path
        if self.conn.table_exists("_quadra_views")? && self.conn.query("SELECT t.\"origin\" FROM \"_quadra_views\" t WHERE 1 = 0", &[]).is_err() {
            self.conn.batch("ALTER TABLE \"_quadra_views\" ADD COLUMN \"origin\" TEXT")?;
            self.conn.batch("ALTER TABLE \"_quadra_views\" ADD COLUMN \"path\" TEXT")?;
        }
        for s in system_ddl(self.dialect()) {
            let name = s.split('"').nth(1).unwrap_or("").to_string();
            if !self.conn.table_exists(&name)? {
                self.conn.batch(&s)?;
            }
        }
        for s in system_indexes_sql() {
            self.conn.batch(&s)?;
        }
        self.set_meta("system", SYSTEM_VERSION)
    }

    pub fn dialect(&self) -> Dialect {
        self.conn.dialect()
    }

    pub fn meta(&mut self, key: &str) -> Result<Option<String>, String> {
        let rows = self.conn.query("SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = ?text", &[Sql::Text(key.into())])?;
        Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.conn.exec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES (?text, ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"", &[Sql::Text(key.into()), Sql::Text(value.into())])?;
        Ok(())
    }

    pub fn status(&mut self) -> Result<Status, String> {
        let fingerprint = self.schema.fingerprint();
        if !self.conn.table_exists("_quadra_meta")? {
            return Ok(Status { initialized: false, schema_current: false, stored_fingerprint: None, fingerprint, commits: 0, last_hash: None });
        }
        let stored = self.meta("schema_fingerprint")?;
        let rows = self.conn.query("SELECT CAST(COUNT(*) AS TEXT), CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\"", &[])?;
        let commits = num(&rows[0][0]);
        let last = rows[0][1].clone();
        let last_hash = match last {
            Some(tx) => self.conn.query("SELECT CAST(\"hash\" AS TEXT) FROM \"_quadra_commits\" WHERE \"tx\" = ?bigint", &[Sql::Int(tx.parse().unwrap_or(0))])?.into_iter().next().and_then(|r| r[0].clone()),
            None => None,
        };
        Ok(Status { initialized: true, schema_current: stored.as_deref() == Some(fingerprint.as_str()), stored_fingerprint: stored, fingerprint, commits, last_hash })
    }

    /// Create system tables and projections in an empty database.
    pub fn init(&mut self) -> Result<(), String> {
        if self.conn.table_exists("_quadra_meta")? {
            return Err("the database is already initialized".into());
        }
        self.conn.batch("BEGIN")?;
        let r = (|| -> Result<(), String> {
            for s in system_ddl(self.dialect()) {
                self.conn.batch(&s)?;
            }
            for s in system_indexes_sql() {
                self.conn.batch(&s)?;
            }
            self.set_meta("system", SYSTEM_VERSION)?;
            for s in self.schema.ddl(self.dialect()) {
                self.conn.batch(&s)?;
            }
            self.sync_indexes(None, false)?;
            let fp = self.schema.fingerprint();
            self.set_meta("schema_fingerprint", &fp)?;
            let names: Vec<String> = self.schema.tables.iter().map(|t| t.name.clone()).collect();
            self.set_meta("schema_tables", &serde_json::to_string(&names).unwrap())?;
            Ok(())
        })();
        self.finish(r)
    }

    /// Undo `init` on a database that has no commit yet (the seeds failed).
    pub fn uninit(&mut self) -> Result<(), String> {
        if !self.conn.table_exists("_quadra_meta")? {
            return Ok(());
        }
        if self.status()?.commits > 0 {
            return Err("the database has commits: it is not removed".into());
        }
        let cascade = if self.dialect() == Dialect::Postgres { " CASCADE" } else { "" };
        self.conn.batch("BEGIN")?;
        let r = (|| -> Result<(), String> {
            let mut names: Vec<String> = self.schema.tables.iter().rev().map(|t| t.name.clone()).collect();
            names.extend(SYSTEM_DDL.iter().rev().filter_map(|s| s.split('"').nth(1).map(str::to_string)));
            if self.dialect() == Dialect::Sqlite {
                self.conn.batch("PRAGMA defer_foreign_keys = ON")?;
            }
            for n in names {
                self.conn.batch(&format!("DROP TABLE IF EXISTS {}{cascade}", q(&n)))?;
            }
            Ok(())
        })();
        self.finish(r)
    }

    pub(crate) fn finish(&mut self, r: Result<(), String>) -> Result<(), String> {
        match r {
            Ok(()) => self.conn.batch("COMMIT"),
            Err(e) => {
                let _ = self.conn.batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ---- indexes ----------------------------------------------------------------------

    /// The `qx_` indexes of the projection tables in the database: (name, table, valid). An
    /// index PostgreSQL left invalid (a concurrent build interrupted) is not valid.
    pub fn existing_indexes(&mut self) -> Result<Vec<(String, String, bool)>, String> {
        let rows = match self.dialect() {
            Dialect::Sqlite => self.conn.query("SELECT \"name\", \"tbl_name\", '1' FROM \"sqlite_master\" WHERE \"type\" = 'index' AND \"name\" LIKE 'qx\\_%' ESCAPE '\\'", &[])?,
            Dialect::Postgres => self.conn.query(
                "SELECT CAST(c.relname AS TEXT), CAST(t.relname AS TEXT), CASE WHEN i.indisvalid THEN '1' ELSE '0' END FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_class t ON t.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = current_schema() AND c.relname LIKE 'qx\\_%' ESCAPE '\\'",
                &[],
            )?,
        };
        Ok(rows.into_iter().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), r[2].as_deref() == Some("1"))).filter(|(_, t, _)| !t.starts_with("_quadra_")).collect())
    }

    /// Bring the indexes of the projection tables in step with the model: create those
    /// missing or invalid, drop the `qx_` indexes no longer wanted. Only the tables in
    /// `tables`, when given. Outside a transaction: on PostgreSQL the indexes are built
    /// `CONCURRENTLY` (writes go on meanwhile); an interrupted build leaves an invalid index,
    /// rebuilt by the next call. Returns (created, dropped).
    pub fn ensure_indexes(&mut self, tables: Option<&std::collections::BTreeSet<String>>) -> Result<(Vec<String>, Vec<String>), String> {
        self.sync_indexes(tables, self.dialect() == Dialect::Postgres)
    }

    /// Like `ensure_indexes`, inside the caller's transaction (no `CONCURRENTLY`).
    pub(crate) fn sync_indexes(&mut self, tables: Option<&std::collections::BTreeSet<String>>, concurrently: bool) -> Result<(Vec<String>, Vec<String>), String> {
        let wanted = |t: &str| tables.map_or(true, |ts| ts.contains(t));
        if self.dialect() == Dialect::Sqlite {
            self.sync_search(tables)?;
        }
        let existing = self.existing_indexes()?;
        let conc = if concurrently { "CONCURRENTLY " } else { "" };
        let mut dropped = Vec::new();
        for (name, table, valid) in &existing {
            if !wanted(table) {
                continue;
            }
            if !*valid || !self.schema.indexes.iter().any(|i| &i.name == name) {
                self.conn.batch(&format!("DROP INDEX {conc}IF EXISTS \"{name}\""))?;
                dropped.push(name.clone());
            }
        }
        let mut created = Vec::new();
        let indexes = self.schema.indexes.clone();
        let sqlite = self.dialect() == Dialect::Sqlite;
        for i in indexes.iter().filter(|i| wanted(&i.table) && !(sqlite && i.fts)) {
            if existing.iter().any(|(n, _, valid)| *n == i.name && *valid) {
                continue;
            }
            let sql = i.create_sql(self.dialect(), concurrently);
            self.conn.batch(&sql)?;
            if i.fts {
                // no statistics of the words (`ANALYZE` would compute them for every sample row:
                // a third of an import's analysis of the table); the planner's default
                // selectivity of `@@` is enough to use the index
                self.conn.batch(&format!("ALTER INDEX \"{}\" ALTER COLUMN 1 SET STATISTICS 0", i.name))?;
            }
            created.push(i.name.clone());
        }
        Ok((created, dropped))
    }

    /// SQLite: the full-text search of each table (`Schema::search_columns`) in an FTS5 table
    /// without content (`schema::fts_table`, columns `c0`, `c1`…, holding `quadra_words` of
    /// each column, by the record's id), kept by three triggers on the table. When the columns
    /// changed (`_quadra_meta` key `search:<table>`), a trigger is missing (a table copied by a
    /// deploy, an import that dropped them) or the FTS table is, it is built again from the
    /// table. Only the tables in `tables`, when given; the FTS tables of tables without search
    /// columns go.
    pub fn sync_search(&mut self, tables: Option<&std::collections::BTreeSet<String>>) -> Result<(), String> {
        let wanted = |t: &str| tables.map_or(true, |ts| ts.contains(t));
        let present: Vec<(String, String)> = self
            .conn
            .query("SELECT \"type\", \"name\" FROM \"sqlite_master\" WHERE \"name\" LIKE 'qx\\_%\\_fts%' ESCAPE '\\'", &[])?
            .into_iter()
            .map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default()))
            .collect();
        let has = |kind: &str, name: &str| present.iter().any(|(k, n)| k == kind && n == name);
        let entity_tables: Vec<String> = self.schema.tables.iter().filter(|t| matches!(t.source, crate::schema::Source::Entity(_))).map(|t| t.name.clone()).collect();
        // FTS tables of tables no longer searched (or gone)
        for (kind, name) in &present {
            if kind != "table" || !name.ends_with("_fts") {
                continue;
            }
            let base = name.trim_start_matches("qx_").trim_end_matches("_fts").to_string();
            let keep = entity_tables.iter().any(|t| *t == base && !self.schema.search_columns(t).is_empty());
            if !keep && (wanted(&base) || !entity_tables.contains(&base)) {
                self.drop_search(&base)?;
                self.conn.exec("DELETE FROM \"_quadra_meta\" WHERE \"key\" = ?text", &[Sql::Text(format!("search:{base}"))])?;
            }
        }
        for t in entity_tables.iter().filter(|t| wanted(t)) {
            let cols = self.schema.search_columns(t);
            if cols.is_empty() {
                continue;
            }
            let fts = crate::schema::fts_table(t);
            let sig = serde_json::to_string(&cols).unwrap();
            let current = self.meta(&format!("search:{t}"))?.as_deref() == Some(sig.as_str()) && has("table", &fts) && ["ai", "ad", "au"].iter().all(|k| has("trigger", &format!("{fts}_{k}")));
            if current {
                continue;
            }
            self.drop_search(t)?;
            let c: Vec<String> = (0..cols.len()).map(|i| format!("c{i}")).collect();
            self.conn.batch(&format!("CREATE VIRTUAL TABLE {} USING fts5({}, content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 0')", q(&fts), c.join(", ")))?;
            let words = |row: &str| cols.iter().map(|x| format!("quadra_words({row}.{})", q(x))).collect::<Vec<_>>().join(", ");
            self.conn.batch(&format!("INSERT INTO {}(\"rowid\", {}) SELECT \"id\", {} FROM {}", q(&fts), c.join(", "), words(q(t).as_str()), q(t)))?;
            let ins = format!("INSERT INTO {}(\"rowid\", {}) VALUES (new.\"id\", {});", q(&fts), c.join(", "), words("new"));
            let del = format!("DELETE FROM {} WHERE \"rowid\" = old.\"id\";", q(&fts));
            self.conn.batch(&format!("CREATE TRIGGER {} AFTER INSERT ON {} BEGIN {ins} END", q(&format!("{fts}_ai")), q(t)))?;
            self.conn.batch(&format!("CREATE TRIGGER {} AFTER DELETE ON {} BEGIN {del} END", q(&format!("{fts}_ad")), q(t)))?;
            let of = cols.iter().map(|x| q(x)).collect::<Vec<_>>().join(", ");
            self.conn.batch(&format!("CREATE TRIGGER {} AFTER UPDATE OF {of} ON {} BEGIN {del} {ins} END", q(&format!("{fts}_au")), q(t)))?;
            self.set_meta(&format!("search:{t}"), &sig)?;
        }
        Ok(())
    }

    /// SQLite: the FTS5 table of a table and its triggers, dropped (an import fills the table
    /// first, a deploy drops or changes its columns: `sync_search` builds them again).
    pub fn drop_search(&mut self, table: &str) -> Result<(), String> {
        drop_search_triggers(&mut self.conn, table)?;
        self.conn.batch(&format!("DROP TABLE IF EXISTS {}", q(&crate::schema::fts_table(table))))
    }

    // ---- loading ----------------------------------------------------------------------

    /// Rebuild the engine's store from the truth tables.
    pub fn load(&mut self) -> Result<Store, String> {
        let mut store = Store::default();
        for r in self.conn.query(
            "SELECT CAST(\"id\" AS TEXT), \"entity\", CAST(\"created_tx\" AS TEXT), \"created_at\", CAST(\"created_by\" AS TEXT), CAST(\"frozen_tx\" AS TEXT), \"cause\", \"warnings\" FROM \"_quadra_records\" ORDER BY \"_quadra_records\".\"id\"",
            &[],
        )? {
            let cause = cause_from_json(&serde_json::from_str(r[6].as_deref().unwrap_or("[]")).unwrap_or(J::Null));
            let warnings: Vec<String> = serde_json::from_str(r[7].as_deref().unwrap_or("[]")).unwrap_or_default();
            store.insert(Record {
                ent: Rc::from(r[1].as_deref().unwrap_or("")),
                id: num(&r[0]) as u64,
                fields: Default::default(),
                history: Vec::new(),
                created_tx: num(&r[2]) as u64,
                created_at: chrono::NaiveDateTime::parse_from_str(r[3].as_deref().unwrap_or(""), "%Y-%m-%dT%H:%M:%S").unwrap_or_default(),
                created_by: r[4].as_deref().and_then(|s| s.parse().ok()),
                pending_defaults: Vec::new(),
                frozen_at: r[5].as_deref().and_then(|s| s.parse().ok()),
                cause,
                warnings,
                partial: None,
                formulas: HashMap::new(),
            });
        }
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), \"entity\", \"old_name\", \"new_name\" FROM \"_quadra_renames\" ORDER BY \"_quadra_renames\".\"seq\"", &[])? {
            store.renames.push((r[1].clone().unwrap_or_default(), r[2].clone().unwrap_or_default(), r[3].clone().unwrap_or_default(), num(&r[0]) as u64));
        }
        for r in self.conn.query("SELECT \"key\", CAST(\"tx\" AS TEXT) FROM \"_quadra_applied\"", &[])? {
            store.applied.insert(r[0].clone().unwrap_or_default(), num(&r[1]) as u64);
        }
        for r in self.conn.query("SELECT CAST(\"seq\" AS TEXT), CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\" FROM \"_quadra_history\" ORDER BY \"_quadra_history\".\"seq\"", &[])? {
            self.seq = self.seq.max(num(&r[0]));
            let v = from_json(&serde_json::from_str(r[4].as_deref().unwrap_or("null")).unwrap_or(J::Null));
            if let Some(rec) = store.records.get_mut(&(num(&r[2]) as u64)) {
                let field = renamed(&store.renames, &rec.ent, num(&r[1]) as u64, r[3].clone().unwrap_or_default());
                rec.fields.insert(field.clone(), v.clone());
                rec.history.push((num(&r[1]) as u64, field, v));
            }
        }
        for r in self.conn.query("SELECT CAST(\"id\" AS TEXT), \"ledger\", CAST(\"tx\" AS TEXT), \"date\", \"source\", \"cols\" FROM \"_quadra_ledger_rows\" ORDER BY \"_quadra_ledger_rows\".\"id\"", &[])? {
            let cols: J = serde_json::from_str(r[5].as_deref().unwrap_or("{}")).unwrap_or(J::Null);
            let cols = cols.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect()).unwrap_or_default();
            let source = from_json(&serde_json::from_str(r[4].as_deref().unwrap_or("null")).unwrap_or(J::Null));
            store.rows.push(LedgerRow {
                ledger: Rc::from(r[1].as_deref().unwrap_or("")),
                id: num(&r[0]) as u64,
                cols,
                source: if source.is_none() { None } else { Some(source) },
                date: chrono::NaiveDateTime::parse_from_str(r[3].as_deref().unwrap_or(""), "%Y-%m-%dT%H:%M:%S").unwrap_or_default(),
                tx: num(&r[2]) as u64,
            });
        }
        for r in self.conn.query("SELECT CAST(\"record_id\" AS TEXT), \"ledger_key\", \"rows\" FROM \"_quadra_postings\"", &[])? {
            let rows: J = serde_json::from_str(r[2].as_deref().unwrap_or("[]")).unwrap_or(J::Null);
            let rows = rows.as_array().map(|a| a.iter().map(|o| o.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect()).unwrap_or_default()).collect()).unwrap_or_default();
            store.postings.insert((num(&r[0]) as u64, r[1].clone().unwrap_or_default()), rows);
        }
        for r in self.conn.query("SELECT \"key\", CAST(\"n\" AS TEXT) FROM \"_quadra_counters\"", &[])? {
            store.counters.insert(r[0].clone().unwrap_or_default(), num(&r[1]) as u64);
        }
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT) FROM \"_quadra_commits\"", &[])? {
            if let Ok(at) = chrono::NaiveDateTime::parse_from_str(r[1].as_deref().unwrap_or(""), "%Y-%m-%dT%H:%M:%S") {
                store.commits.insert(num(&r[0]) as u64, (at, r[2].as_deref().and_then(|u| u.parse().ok())));
            }
        }
        if let Some(state) = self.meta("state")? {
            let s: J = serde_json::from_str(&state).unwrap_or(J::Null);
            store.next_id = s["next_id"].as_u64().unwrap_or(0);
            store.tx = s["tx"].as_u64().unwrap_or(0);
        }
        // ids reserved by engines loading on demand come after
        let reserved = self.meta("id_reserved")?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        store.next_id = store.next_id.max(reserved);
        store.base = store.tx;
        let last = self.conn.query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1", &[])?;
        if let Some(h) = last.into_iter().next().and_then(|r| r[0].clone()) {
            self.prev_hash = h;
        }
        Ok(store)
    }

    // ---- projections ------------------------------------------------------------------

    pub(crate) fn encode(&self, c: &Column, v: &V) -> Sql {
        encode(c, v)
    }

    pub(crate) fn upsert(&mut self, table: &str, cols: &[(String, ColType, Sql)], key: &[&str]) -> Result<(), String> {
        let names: Vec<String> = cols.iter().map(|(n, _, _)| q(n)).collect();
        let params: Vec<&str> = cols.iter().map(|(_, t, _)| t.param()).collect();
        let updates: Vec<String> = cols.iter().filter(|(n, _, _)| !key.contains(&n.as_str())).map(|(n, _, _)| format!("{} = excluded.{}", q(n), q(n))).collect();
        let conflict = if updates.is_empty() { "DO NOTHING".to_string() } else { format!("DO UPDATE SET {}", updates.join(", ")) };
        let sql = format!("INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) {conflict}", q(table), names.join(", "), params.join(", "), key.iter().map(|k| q(k)).collect::<Vec<_>>().join(", "));
        let values: Vec<Sql> = cols.iter().map(|(_, _, v)| v.clone()).collect();
        self.wexec(sql, values)
    }

    /// A write: sent now, or held with the other writes of the commit (`deferred`).
    pub(crate) fn wexec(&mut self, sql: String, params: Vec<Sql>) -> Result<(), String> {
        match &mut self.deferred {
            Some(list) => {
                list.push((sql, params));
                if list.iter().map(|(_, p)| p.len()).sum::<usize>() > 20_000 {
                    self.flush()?;
                }
                Ok(())
            }
            None => self.conn.exec(&sql, &params).map(|_| ()),
        }
    }

    /// The writes held, in one statement: `WITH q1 AS (…), q2 AS (…) SELECT 1` (they touch
    /// different rows; PostgreSQL runs every data-modifying `WITH`).
    pub(crate) fn flush(&mut self) -> Result<(), String> {
        let Some(list) = self.deferred.as_mut() else {
            return Ok(());
        };
        let list = std::mem::take(list);
        match list.len() {
            0 => Ok(()),
            1 => self.conn.exec(&list[0].0, &list[0].1).map(|_| ()),
            _ => {
                let mut sql = String::from("WITH ");
                let mut params = Vec::new();
                for (i, (s, p)) in list.into_iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str(&format!("\"_q{i}\" AS ({s})"));
                    params.extend(p);
                }
                sql.push_str(" SELECT 1");
                self.conn.exec(&sql, &params).map(|_| ())
            }
        }
    }

    fn field_value(store: &Store, rec: &Record, fm: &FieldMap) -> V {
        if fm.formula {
            store.materialized.get(&(rec.id, fm.field.clone())).cloned().unwrap_or(V::None)
        } else {
            rec.fields.get(&fm.field).cloned().unwrap_or(V::None)
        }
    }

    /// Write the projection rows of one record (entity table, trait tables, sets, per company).
    pub(crate) fn project_record(&mut self, model: &Model, store: &Store, id: u64, map: &dyn Fn(u64) -> u64) -> Result<(), String> {
        if let Some((table, cols)) = self.entity_row(store, id, map) {
            self.upsert(&table, &cols, &["id"])?;
        }
        self.project_extras(model, store, id, None)
    }

    /// The row of a record in its entity table: (table, columns with their values). Formulas
    /// only when computed (with storage on demand only those that may have changed were).
    fn entity_row(&self, store: &Store, id: u64, map: &dyn Fn(u64) -> u64) -> Option<(String, Vec<(String, ColType, Sql)>)> {
        let rec = store.records.get(&id)?;
        let &ti = self.entity_tables.get(&*rec.ent)?;
        let table = &self.schema.tables[ti];
        let mut cols: Vec<(String, ColType, Sql)> = vec![
            ("id".into(), ColType::BigInt, Sql::Int(id as i64)),
            ("_created_tx".into(), ColType::BigInt, Sql::Int(map(rec.created_tx) as i64)),
            ("_created_at".into(), ColType::Timestamp, Sql::Text(rec.created_at.format("%Y-%m-%d %H:%M:%S").to_string())),
            ("_created_by".into(), ColType::BigInt, rec.created_by.map(|u| Sql::Int(u as i64)).unwrap_or(Sql::Null)),
            ("_frozen_tx".into(), ColType::BigInt, rec.frozen_at.map(|t| Sql::Int(map(t) as i64)).unwrap_or(Sql::Null)),
        ];
        for fm in &table.fields {
            if fm.formula && !store.materialized.contains_key(&(id, fm.field.clone())) {
                continue;
            }
            let v = Self::field_value(store, rec, fm);
            for c in &fm.columns {
                cols.push((c.name.clone(), c.ty, self.encode(c, &v)));
            }
        }
        Some((table.name.clone(), cols))
    }

    /// The other projection rows of a record: its trait tables, join tables, per-company
    /// tables. `written`: only the sets and per-company fields this transaction wrote (all of
    /// them when the record was created in it); None: all of them.
    fn project_extras(&mut self, model: &Model, store: &Store, id: u64, written: Option<u64>) -> Result<(), String> {
        let Some(rec) = store.records.get(&id) else {
            return Ok(());
        };
        let ent = rec.ent.to_string();
        if !self.entity_tables.contains_key(&ent) {
            return Ok(());
        }
        let fresh = written.map_or(true, |tx| rec.created_tx == tx);
        let wrote = |field: &str| fresh || written.is_some_and(|tx| rec.history.iter().any(|(t, f, _)| *t == tx && (f == field || f.starts_with(&format!("{field}@")))));
        // trait tables
        if fresh {
            if let Some(e) = model.entity(&ent) {
                let mut stack: Vec<String> = e.traits.clone();
                while let Some(t) = stack.pop() {
                    let tname = schema::snake(&t);
                    if self.schema.table(&tname).map_or(false, |x| matches!(x.source, Source::Trait(_))) {
                        self.upsert(&tname, &[("id".into(), ColType::BigInt, Sql::Int(id as i64)), ("entity".into(), ColType::Text, Sql::Text(ent.clone()))], &["id"])?;
                    }
                    if let Some(te) = model.entity(&t) {
                        stack.extend(te.traits.iter().cloned());
                    }
                }
            }
        }
        // join tables and per-company tables of this entity
        let extra: Vec<Table> = self.schema.tables.iter().filter(|t| matches!(&t.source, Source::Join(e, f) | Source::PerCompany(e, f) if *e == ent && wrote(f))).cloned().collect();
        for t in extra {
            match &t.source {
                Source::Join(_, field) => {
                    self.conn.exec(&format!("DELETE FROM {} WHERE \"owner_id\" = ?bigint", q(&t.name)), &[Sql::Int(id as i64)])?;
                    let items = rec.fields.get(field).and_then(|v| v.items().map(|i| i.to_vec())).unwrap_or_default();
                    // one row per reference, at its last position (a reference given twice
                    // would be the same row twice in one statement)
                    let mut last: HashMap<u64, usize> = HashMap::new();
                    for (n, it) in items.iter().enumerate() {
                        if let V::Ref { id: target, .. } = it {
                            last.insert(*target, n);
                        }
                    }
                    for (n, it) in items.into_iter().enumerate() {
                        if let V::Ref { id: target, .. } = it {
                            if last.get(&target) != Some(&n) {
                                continue;
                            }
                            self.upsert(
                                &t.name,
                                &[("owner_id".into(), ColType::BigInt, Sql::Int(id as i64)), ("target_id".into(), ColType::BigInt, Sql::Int(target as i64)), ("ord".into(), ColType::BigInt, Sql::Int(n as i64))],
                                &["owner_id", "target_id"],
                            )?;
                        }
                    }
                }
                Source::PerCompany(_, field) => {
                    let prefix = format!("{field}@");
                    let fm = t.fields[0].clone();
                    for (k, v) in rec.fields.iter().filter(|(k, _)| k.starts_with(&prefix)) {
                        let company: i64 = k[prefix.len()..].parse().unwrap_or(0);
                        if company == 0 {
                            continue;
                        }
                        let mut cols = vec![("owner_id".to_string(), ColType::BigInt, Sql::Int(id as i64)), ("company_id".to_string(), ColType::BigInt, Sql::Int(company))];
                        for c in &fm.columns {
                            cols.push((c.name.clone(), c.ty, self.encode(c, v)));
                        }
                        self.upsert(&t.name, &cols, &["owner_id", "company_id"])?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Many rows into a table, a few hundred per statement (`INSERT … VALUES (…), (…)`); an
    /// upsert on `key` when given (every other column is updated).
    pub(crate) fn insert_rows(&mut self, table: &str, cols: &[(String, ColType)], rows: &[Vec<Sql>], key: Option<&[&str]>) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let names: Vec<String> = cols.iter().map(|(n, _)| q(n)).collect();
        let one = format!("({})", cols.iter().map(|(_, t)| t.param()).collect::<Vec<_>>().join(", "));
        let conflict = match key {
            Some(key) => {
                let updates: Vec<String> = cols.iter().filter(|(n, _)| !key.contains(&n.as_str())).map(|(n, _)| format!("{} = excluded.{}", q(n), q(n))).collect();
                let action = if updates.is_empty() { "DO NOTHING".to_string() } else { format!("DO UPDATE SET {}", updates.join(", ")) };
                format!(" ON CONFLICT ({}) {action}", key.iter().map(|k| q(k)).collect::<Vec<_>>().join(", "))
            }
            None => String::new(),
        };
        let per = (2000 / cols.len().max(1)).max(1);
        for chunk in rows.chunks(per) {
            let sql = format!("INSERT INTO {} ({}) VALUES {}{conflict}", q(table), names.join(", "), vec![one.as_str(); chunk.len()].join(", "));
            let params: Vec<Sql> = chunk.iter().flat_map(|r| r.iter().cloned()).collect();
            self.wexec(sql, params)?;
        }
        Ok(())
    }

    /// Update columns of many rows by id, a few hundred per statement (`WITH v AS (VALUES …)
    /// UPDATE … FROM v`): `cols` are the columns set, each row is the id then their values.
    pub(crate) fn update_rows(&mut self, table: &str, cols: &[(String, ColType)], rows: &[Vec<Sql>]) -> Result<(), String> {
        if rows.is_empty() || cols.is_empty() {
            return Ok(());
        }
        let mut all: Vec<(String, ColType)> = vec![("id".into(), ColType::BigInt)];
        all.extend(cols.iter().cloned());
        let names: Vec<String> = all.iter().map(|(n, _)| q(n)).collect();
        let one = format!("({})", all.iter().map(|(_, t)| t.param()).collect::<Vec<_>>().join(", "));
        let set: Vec<String> = cols.iter().map(|(n, _)| format!("{} = \"_v\".{}", q(n), q(n))).collect();
        let per = (2000 / all.len()).max(1);
        for chunk in rows.chunks(per) {
            let sql = if chunk.len() == 1 {
                let set1: Vec<String> = cols.iter().map(|(n, t)| format!("{} = {}", q(n), t.param())).collect();
                format!("UPDATE {} SET {} WHERE \"id\" = ?bigint", q(table), set1.join(", "))
            } else if self.dialect() == Dialect::Postgres {
                // (no `WITH` of its own: it may be one of a commit's `WITH`s, `flush`)
                format!("UPDATE {} SET {} FROM (VALUES {}) AS \"_v\" ({}) WHERE {}.\"id\" = \"_v\".\"id\"", q(table), set.join(", "), vec![one.as_str(); chunk.len()].join(", "), names.join(", "), q(table))
            } else {
                format!("WITH \"_v\" ({}) AS (VALUES {}) UPDATE {} SET {} FROM \"_v\" WHERE {}.\"id\" = \"_v\".\"id\"", names.join(", "), vec![one.as_str(); chunk.len()].join(", "), q(table), set.join(", "), q(table))
            };
            let params: Vec<Sql> = if chunk.len() == 1 {
                let r = &chunk[0];
                r[1..].iter().cloned().chain(std::iter::once(r[0].clone())).collect()
            } else {
                chunk.iter().flat_map(|r| r.iter().cloned()).collect()
            };
            self.wexec(sql, params)?;
        }
        Ok(())
    }

    pub(crate) fn project_row(&mut self, row: &LedgerRow) -> Result<(), String> {
        let Some(&ti) = self.entity_tables.get(&*row.ledger) else {
            return Ok(());
        };
        let table = self.schema.tables[ti].clone();
        let (src_ent, src_id) = match &row.source {
            Some(V::Ref { ent, id }) => (Sql::Text(ent.to_string()), Sql::Int(*id as i64)),
            _ => (Sql::Null, Sql::Null),
        };
        let mut cols: Vec<(String, ColType, Sql)> = vec![
            ("id".into(), ColType::BigInt, Sql::Int(row.id as i64)),
            ("_tx".into(), ColType::BigInt, Sql::Int(row.tx as i64)),
            ("_date".into(), ColType::Timestamp, Sql::Text(row.date.format("%Y-%m-%d %H:%M:%S").to_string())),
            ("_source_entity".into(), ColType::Text, src_ent),
            ("_source_id".into(), ColType::BigInt, src_id),
        ];
        for fm in &table.fields {
            let v = row.cols.get(&fm.field).cloned().unwrap_or(V::None);
            for c in &fm.columns {
                cols.push((c.name.clone(), c.ty, self.encode(c, &v)));
            }
        }
        self.upsert(&table.name, &cols, &["id"])
    }

    /// The record's row of `_quadra_records`: `last_tx` the transaction writing it, `version_tx`
    /// the last one that wrote its fields.
    pub(crate) fn record_meta(&mut self, rec: &Record, last_tx: u64, version_tx: u64) -> Result<(), String> {
        let version_tx = if exact_row(&self.schema, rec) { version_tx } else { 0 };
        let kinds = kinds_of(&self.schema, rec);
        let row = Self::meta_row(rec, last_tx, version_tx, kinds, &|t| t);
        self.insert_rows("_quadra_records", &Self::meta_cols(), &[row], Some(&["id"]))
    }

    fn meta_cols() -> Vec<(String, ColType)> {
        [
            ("last_tx", ColType::BigInt),
            ("version_tx", ColType::BigInt),
            ("id", ColType::BigInt),
            ("entity", ColType::Text),
            ("created_tx", ColType::BigInt),
            ("created_at", ColType::Text),
            ("created_by", ColType::BigInt),
            ("frozen_tx", ColType::BigInt),
            ("cause", ColType::Text),
            ("warnings", ColType::Text),
            ("kinds", ColType::Text),
        ]
        .iter()
        .map(|(n, t)| (n.to_string(), *t))
        .collect()
    }

    /// The values of `meta_cols` for a record (`map`: its transactions as the database numbers them).
    fn meta_row(rec: &Record, last_tx: u64, version_tx: u64, kinds: Option<String>, map: &dyn Fn(u64) -> u64) -> Vec<Sql> {
        vec![
            Sql::Int(last_tx as i64),
            Sql::Int(version_tx as i64),
            Sql::Int(rec.id as i64),
            Sql::Text(rec.ent.to_string()),
            Sql::Int(map(rec.created_tx) as i64),
            Sql::Text(rec.created_at.format("%Y-%m-%dT%H:%M:%S").to_string()),
            rec.created_by.map(|u| Sql::Int(u as i64)).unwrap_or(Sql::Null),
            rec.frozen_at.map(|t| Sql::Int(map(t) as i64)).unwrap_or(Sql::Null),
            Sql::Text(cause_to_json(&rec.cause).to_string()),
            Sql::Text(serde_json::to_string(&rec.warnings).unwrap()),
            kinds.map(Sql::Text).unwrap_or(Sql::Null),
        ]
    }

    /// Write one engine transaction: truth, projections and the signed commit row.
    /// Write a transaction: `c.tx` is the engine's number, `tx` the one the database assigned.
    fn write_tx(&mut self, model: &Model, c: &Commit, tx: u64, stored_next: u64) -> Result<(), String> {
        // PostgreSQL: the writes go in a few statements of data-modifying `WITH`s
        if self.dialect() == Dialect::Postgres {
            self.deferred = Some(Vec::new());
        }
        let r = self.write_tx_inner(model, c, tx, stored_next);
        self.deferred = None;
        r
    }

    fn write_tx_inner(&mut self, model: &Model, c: &Commit, tx: u64, stored_next: u64) -> Result<(), String> {
        let local = c.tx;
        let map = |t: u64| if t == local { tx } else { t };
        let mut payload: Vec<J> = Vec::new();
        let mut touched: HashSet<u64> = HashSet::new();
        // history entries of this transaction
        let mut entries: Vec<(u64, String, V)> = Vec::new();
        for r in c.after.records.values() {
            for (t, f, v) in &r.history {
                if *t == local {
                    entries.push((r.id, f.clone(), v.clone()));
                }
            }
        }
        // canonical order: by record, then field (writes to one field keep their order), so
        // the hash does not depend on when the engine happened to compute a value
        entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut hist: Vec<Vec<Sql>> = Vec::with_capacity(entries.len());
        for (id, f, v) in &entries {
            self.seq += 1;
            let value = to_json(v).to_string();
            payload.push(json!(["h", id, f, value]));
            hist.push(vec![Sql::Int(self.seq), Sql::Int(tx as i64), Sql::Int(*id as i64), Sql::Text(f.clone()), Sql::Text(value)]);
            touched.insert(*id);
        }
        let hcols: Vec<(String, ColType)> =
            [("seq", ColType::BigInt), ("tx", ColType::BigInt), ("record_id", ColType::BigInt), ("field", ColType::Text), ("value", ColType::Text)].iter().map(|(n, t)| (n.to_string(), *t)).collect();
        self.insert_rows("_quadra_history", &hcols, &hist, None)?;
        // records created or changed (meta, freeze, warnings, cause)
        for r in c.after.records.values() {
            let changed = match (c.before.records.get(&r.id), c.after.origin.get(&r.id)) {
                (Some(b), _) => b.frozen_at != r.frozen_at || b.warnings != r.warnings || !Rc::ptr_eq(&b.cause, &r.cause),
                // loaded during the transaction: as it was loaded, unless changed since
                (None, Some((frozen, warnings, cause))) => r.created_tx == local || *frozen != r.frozen_at || *warnings != r.warnings || !Rc::ptr_eq(cause, &r.cause),
                // created here
                (None, None) => true,
            };
            if changed {
                touched.insert(r.id);
            }
        }
        let mut ids: Vec<u64> = touched.iter().copied().collect();
        ids.sort();
        let mut metas: Vec<Vec<Sql>> = Vec::new();
        for id in &ids {
            if let Some(r) = c.after.records.get(id) {
                let version = if exact_row(&self.schema, r) { map(r.version()) } else { 0 };
                metas.push(Self::meta_row(r, tx, version, kinds_of(&self.schema, r), &map));
            }
        }
        self.insert_rows("_quadra_records", &Self::meta_cols(), &metas, Some(&["id"]))?;
        // projections: the rows of the records touched, the formula columns of the records
        // whose materialized formulas changed (compared with the database's values)
        let mut changed_formulas: std::collections::BTreeMap<u64, Vec<String>> = Default::default();
        for (k, v) in &c.after.materialized {
            let stored = c.after.records.get(&k.0).and_then(|r| r.formulas.get(&k.1)).or_else(|| c.before.materialized.get(k));
            if stored != Some(v) {
                changed_formulas.entry(k.0).or_default().push(k.1.clone());
            }
        }
        // entity rows, one statement per table and set of columns
        let mut rows: std::collections::BTreeMap<(String, Vec<String>), (Vec<(String, ColType)>, Vec<Vec<Sql>>)> = Default::default();
        for id in &ids {
            if let Some((table, cols)) = self.entity_row(c.after, *id, &map) {
                let names: Vec<String> = cols.iter().map(|(n, _, _)| n.clone()).collect();
                let e = rows.entry((table, names)).or_insert_with(|| (cols.iter().map(|(n, t, _)| (n.clone(), *t)).collect(), Vec::new()));
                e.1.push(cols.into_iter().map(|(_, _, v)| v).collect());
            }
        }
        for ((table, _), (cols, vals)) in rows {
            self.insert_rows(&table, &cols, &vals, Some(&["id"]))?;
        }
        for id in &ids {
            self.project_extras(model, c.after, *id, Some(local))?;
        }
        // formula columns only
        let mut updates: std::collections::BTreeMap<(String, Vec<String>), (Vec<(String, ColType)>, Vec<Vec<Sql>>)> = Default::default();
        let mut bumped: Vec<u64> = Vec::new();
        for (id, fields) in &changed_formulas {
            if touched.contains(id) {
                continue;
            }
            let Some(r) = c.after.records.get(id) else {
                continue;
            };
            let Some(&ti) = self.entity_tables.get(&*r.ent) else {
                continue;
            };
            let table = &self.schema.tables[ti];
            let mut cols: Vec<(String, ColType)> = Vec::new();
            let mut vals: Vec<Sql> = vec![Sql::Int(*id as i64)];
            for fm in table.fields.iter().filter(|fm| fm.formula && fields.contains(&fm.field)) {
                let v = c.after.materialized.get(&(*id, fm.field.clone())).cloned().unwrap_or(V::None);
                for col in &fm.columns {
                    cols.push((col.name.clone(), col.ty));
                    vals.push(encode(col, &v));
                }
            }
            if cols.is_empty() {
                continue;
            }
            bumped.push(*id);
            let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
            updates.entry((table.name.clone(), names)).or_insert_with(|| (cols, Vec::new())).1.push(vals);
        }
        for ((table, _), (cols, vals)) in updates {
            self.update_rows(&table, &cols, &vals)?;
        }
        // their rows changed: caches and snapshot checks see it (`last_tx`)
        for chunk in bumped.chunks(500) {
            let list: Vec<String> = chunk.iter().map(|i| i.to_string()).collect();
            self.wexec(format!("UPDATE \"_quadra_records\" SET \"last_tx\" = ?bigint WHERE \"id\" IN ({})", list.join(", ")), vec![Sql::Int(tx as i64)])?;
        }
        // ledger rows
        let before_rows: HashSet<u64> = c.before.rows.iter().map(|r| r.id).collect();
        for row in c.after.rows.iter().filter(|r| r.tx == local && !before_rows.contains(&r.id)) {
            let mut row = row.clone();
            row.tx = tx;
            let cols: serde_json::Map<String, J> = row.cols.iter().map(|(k, v)| (k.clone(), to_json(v))).collect();
            let cols = J::Object(cols).to_string();
            let source = row.source.as_ref().map(to_json).unwrap_or(J::Null).to_string();
            payload.push(json!(["r", row.id, row.ledger.to_string(), cols]));
            self.wexec(
                "INSERT INTO \"_quadra_ledger_rows\" (\"id\", \"ledger\", \"tx\", \"date\", \"source\", \"cols\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text)".into(),
                vec![Sql::Int(row.id as i64), Sql::Text(row.ledger.to_string()), Sql::Int(tx as i64), Sql::Text(row.date.format("%Y-%m-%dT%H:%M:%S").to_string()), Sql::Text(source), Sql::Text(cols)],
            )?;
            self.project_row(&row)?;
        }
        for (k, rows) in &c.after.postings {
            if c.before.postings.get(k) != Some(rows) {
                let js: Vec<J> = rows.iter().map(|r| J::Object(r.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())).collect();
                self.upsert(
                    "_quadra_postings",
                    &[("record_id".into(), ColType::BigInt, Sql::Int(k.0 as i64)), ("ledger_key".into(), ColType::Text, Sql::Text(k.1.clone())), ("rows".into(), ColType::Text, Sql::Text(J::Array(js).to_string()))],
                    &["record_id", "ledger_key"],
                )?;
            }
        }
        for (k, n) in &c.after.counters {
            if c.before.counters.get(k) != Some(n) {
                self.upsert("_quadra_counters", &[("key".into(), ColType::Text, Sql::Text(k.clone())), ("n".into(), ColType::BigInt, Sql::Int(*n as i64))], &["key"])?;
            }
        }
        self.flush()?;
        self.write_scripts(c, &mut payload, &map)?;
        // an API request with an idempotency key: remembered with the commit it produced
        if let Some((user, key, hash, response)) = &c.after.idempotency {
            self.wexec(
                "INSERT INTO \"_quadra_idempotency\" (\"user_id\", \"key\", \"request_hash\", \"response\", \"tx\", \"created_at\") VALUES (?bigint, ?text, ?text, ?text, ?bigint, ?text)".into(),
                vec![Sql::Int(*user as i64), Sql::Text(key.clone()), Sql::Text(hash.clone()), Sql::Text(response.clone()), Sql::Int(tx as i64), Sql::Text(c.time.format("%Y-%m-%dT%H:%M:%S").to_string())],
            )?;
        }
        self.flush()?;
        self.write_outbox(c, tx)?;
        let state = json!({ "next_id": stored_next.max(c.after.next_id), "tx": tx }).to_string();
        self.wexec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('state', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"".into(), vec![Sql::Text(state)])?;
        // signed commit row
        let at = c.time.format("%Y-%m-%dT%H:%M:%S").to_string();
        let hash = commit_hash(&self.prev_hash, tx, &at, c.user, &c.cause, &payload);
        self.wexec(
            "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text)".into(),
            vec![Sql::Int(tx as i64), Sql::Text(at), c.user.map(|u| Sql::Int(u as i64)).unwrap_or(Sql::Null), Sql::Text(c.cause.clone()), Sql::Text(self.prev_hash.clone()), Sql::Text(hash.clone())],
        )?;
        self.flush()?;
        self.prev_hash = hash;
        Ok(())
    }

    /// The events the transaction emitted, in the order of `emit`: pending until an
    /// integration marks them delivered. Not part of the commit hash (the outbox is an
    /// operational queue: its `delivered` column changes).
    pub(crate) fn write_outbox(&mut self, c: &Commit, tx: u64) -> Result<(), String> {
        if c.after.outbox.is_empty() {
            return Ok(());
        }
        let last = self.conn.query("SELECT CAST(MAX(\"id\") AS TEXT) FROM \"_quadra_outbox\"", &[])?;
        let mut id = last.first().map(|r| num(&r[0])).unwrap_or(0);
        let at = c.time.format("%Y-%m-%dT%H:%M:%S").to_string();
        for v in &c.after.outbox {
            let V::Event { name, fields } = v else {
                continue;
            };
            id += 1;
            let fields: serde_json::Map<String, J> = fields.iter().map(|(n, v)| (n.clone(), to_json(v))).collect();
            self.conn.exec(
                "INSERT INTO \"_quadra_outbox\" (\"id\", \"tx\", \"event\", \"fields\", \"created_at\", \"delivered\", \"delivered_at\") VALUES (?bigint, ?bigint, ?text, ?text, ?text, ?boolean, ?text)",
                &[Sql::Int(id), Sql::Int(tx as i64), Sql::Text(name.to_string()), Sql::Text(J::Object(fields).to_string()), Sql::Text(at.clone()), Sql::Bool(false), Sql::Null],
            )?;
        }
        Ok(())
    }

    /// The events of the outbox, oldest first (only those not delivered yet: `pending`).
    pub fn outbox(&mut self, pending: bool) -> Result<Vec<OutboxEvent>, String> {
        let filter = if pending { " WHERE \"delivered_at\" IS NULL" } else { "" };
        let rows = self
            .conn
            .query(&format!("SELECT CAST(\"id\" AS TEXT), CAST(\"tx\" AS TEXT), \"event\", \"fields\", \"created_at\", \"delivered_at\" FROM \"_quadra_outbox\"{filter} ORDER BY \"_quadra_outbox\".\"id\""), &[])?;
        Ok(rows
            .into_iter()
            .map(|r| OutboxEvent {
                id: num(&r[0]) as u64,
                tx: num(&r[1]) as u64,
                event: r[2].clone().unwrap_or_default(),
                fields: r[3].as_deref().and_then(|f| serde_json::from_str(f).ok()).unwrap_or(J::Null),
                created_at: r[4].clone().unwrap_or_default(),
                delivered_at: r[5].clone(),
            })
            .collect())
    }

    /// Mark events delivered (an integration has handed them on). Returns how many were
    /// pending and are now delivered.
    pub fn mark_delivered(&mut self, ids: &[u64], time: chrono::NaiveDateTime) -> Result<usize, String> {
        let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn.batch("BEGIN")?;
        let mut n = 0;
        let r = (|| -> Result<(), String> {
            for id in ids {
                n += self.conn.exec(
                    "UPDATE \"_quadra_outbox\" SET \"delivered\" = ?boolean, \"delivered_at\" = ?text WHERE \"id\" = ?bigint AND \"delivered_at\" IS NULL",
                    &[Sql::Bool(true), Sql::Text(at.clone()), Sql::Int(*id as i64)],
                )? as usize;
            }
            Ok(())
        })();
        self.finish(r)?;
        Ok(n)
    }

    /// Commits after `base` that touched what a transaction read.
    fn find_conflicts(&mut self, base: u64, reads: &ReadSet) -> Result<Vec<Conflict>, String> {
        let b = Sql::Int(base as i64);
        let mut what: std::collections::BTreeMap<u64, Vec<String>> = Default::default();
        let ids: Vec<String> = reads.records.iter().map(|i| i.to_string()).collect();
        for chunk in ids.chunks(400) {
            for r in self.conn.query(
                &format!(
                    "SELECT CAST(h.\"tx\" AS TEXT), CAST(h.\"record_id\" AS TEXT), h.\"field\", r.\"entity\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE h.\"tx\" > ?bigint AND h.\"record_id\" IN ({})",
                    chunk.join(", ")
                ),
                &[b.clone()],
            )? {
                what.entry(num(&r[0]) as u64).or_default().push(format!("{}#{}.{}", r[3].clone().unwrap_or_default(), num(&r[1]), r[2].clone().unwrap_or_default()));
            }
        }
        // materialized formulas read (not computed): their rows changed since (`last_tx`)
        let fids: Vec<String> = reads.formulas.iter().map(|i| i.to_string()).collect();
        for chunk in fids.chunks(400) {
            for r in
                self.conn.query(&format!("SELECT CAST(\"last_tx\" AS TEXT), CAST(\"id\" AS TEXT), \"entity\" FROM \"_quadra_records\" WHERE \"last_tx\" > ?bigint AND \"id\" IN ({})", chunk.join(", ")), &[b.clone()])?
            {
                let (tx, id) = (num(&r[0]) as u64, num(&r[1]));
                let ent = r[2].clone().unwrap_or_default();
                let w = what.entry(tx).or_default();
                if !w.iter().any(|x| x.starts_with(&format!("{ent}#{id}."))) {
                    w.push(format!("{ent}#{id} (its computed values)"));
                }
            }
        }
        for ent in &reads.formula_scans {
            for r in
                self.conn.query("SELECT CAST(\"last_tx\" AS TEXT), CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text AND \"last_tx\" > ?bigint LIMIT 100", &[Sql::Text(ent.clone()), b.clone()])?
            {
                what.entry(num(&r[0]) as u64).or_default().push(format!("{ent}#{} (its computed values)", num(&r[1])));
            }
        }
        for ent in &reads.scans {
            for r in self.conn.query("SELECT CAST(\"created_tx\" AS TEXT), CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text AND \"created_tx\" > ?bigint", &[Sql::Text(ent.clone()), b.clone()])? {
                what.entry(num(&r[0]) as u64).or_default().push(format!("new {ent}#{}", num(&r[1])));
            }
        }
        for (ent, field) in &reads.finds {
            for r in self.conn.query(
                "SELECT CAST(h.\"tx\" AS TEXT), CAST(h.\"record_id\" AS TEXT) FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE h.\"tx\" > ?bigint AND r.\"entity\" = ?text AND h.\"field\" = ?text",
                &[b.clone(), Sql::Text(ent.clone()), Sql::Text(field.clone())],
            )? {
                what.entry(num(&r[0]) as u64).or_default().push(format!("{ent}#{}.{field}", num(&r[1])));
            }
        }
        for (ledger, filter) in &reads.ledgers {
            for r in self.conn.query(&format!("SELECT {} FROM \"_quadra_ledger_rows\" WHERE \"ledger\" = ?text AND \"tx\" > ?bigint", crate::source::ROW_COLS), &[Sql::Text(ledger.clone()), b.clone()])? {
                let row = crate::source::decode_row(&r);
                let hit = match filter {
                    None => true,
                    Some((col, key)) => row.cols.get(col).map_or(false, |v| match v {
                        V::Ref { id, .. } => format!("#{id}") == *key,
                        other => other.to_string() == *key,
                    }),
                };
                if hit {
                    let label = match filter {
                        Some((col, key)) => format!("{ledger} rows with {col} = {key}"),
                        None => format!("{ledger} rows"),
                    };
                    let w = what.entry(row.tx).or_default();
                    if !w.contains(&label) {
                        w.push(label);
                    }
                }
            }
        }
        for (key, seen) in &reads.counters {
            let r = self.conn.query("SELECT CAST(\"n\" AS TEXT) FROM \"_quadra_counters\" WHERE \"key\" = ?text", &[Sql::Text(key.clone())])?;
            let now = r.first().map(|r| num(&r[0]) as u64).unwrap_or(0);
            if now != *seen {
                // the counter has no transaction: attribute it to the last commit
                let head = crate::source::head(&mut self.conn)?;
                what.entry(head).or_default().push(format!("counter {key}"));
            }
        }
        self.describe_commits(what)
    }

    /// Every commit after `base` (a working set loaded in full relies on everything).
    fn commits_after(&mut self, base: u64) -> Result<Vec<Conflict>, String> {
        let mut what: std::collections::BTreeMap<u64, Vec<String>> = Default::default();
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT) FROM \"_quadra_commits\" WHERE \"tx\" > ?bigint", &[Sql::Int(base as i64)])? {
            what.entry(num(&r[0]) as u64).or_default().push("data read by this transaction".into());
        }
        self.describe_commits(what)
    }

    fn describe_commits(&mut self, what: std::collections::BTreeMap<u64, Vec<String>>) -> Result<Vec<Conflict>, String> {
        let mut out = Vec::new();
        for (tx, mut items) in what {
            items.sort();
            items.dedup();
            let r = self.conn.query("SELECT \"at\", CAST(\"user_id\" AS TEXT), \"cause\" FROM \"_quadra_commits\" WHERE \"tx\" = ?bigint", &[Sql::Int(tx as i64)])?;
            let Some(r) = r.first() else { continue };
            let user = match r[1].as_deref().and_then(|u| u.parse::<i64>().ok()) {
                Some(uid) => self
                    .conn
                    .query("SELECT \"value\" FROM \"_quadra_history\" WHERE \"record_id\" = ?bigint AND \"field\" = 'login' ORDER BY \"_quadra_history\".\"seq\" DESC", &[Sql::Int(uid)])?
                    .first()
                    .and_then(|x| x[0].clone())
                    .map(|v| from_json(&serde_json::from_str(&v).unwrap_or(J::Null)).to_string()),
                None => None,
            };
            out.push(Conflict { tx, at: r[0].clone().unwrap_or_default(), user, cause: r[2].clone().unwrap_or_default(), what: items });
        }
        Ok(out)
    }

    /// Migrations and changes applied, and renames, in this transaction.
    pub(crate) fn write_scripts(&mut self, c: &Commit, payload: &mut Vec<J>, map: &dyn Fn(u64) -> u64) -> Result<(), String> {
        let (applied, renames) = script_payload(c.before, c.after);
        let applied: Vec<(String, u64)> = applied.into_iter().map(|(k, t)| (k, map(t))).collect();
        let renames: Vec<(String, String, String, u64)> = renames.into_iter().map(|(e, o, n, t)| (e, o, n, map(t))).collect();
        for (k, t) in applied {
            payload.push(json!(["a", k]));
            self.conn.exec("INSERT INTO \"_quadra_applied\" (\"key\", \"tx\") VALUES (?text, ?bigint)", &[Sql::Text(k), Sql::Int(t as i64)])?;
        }
        for (e, old, new, t) in renames {
            payload.push(json!(["n", e, old, new]));
            let seq = self.conn.query("SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_renames\"", &[])?;
            let seq = num(&seq[0][0]) + 1;
            self.conn.exec(
                "INSERT INTO \"_quadra_renames\" (\"seq\", \"tx\", \"entity\", \"old_name\", \"new_name\") VALUES (?bigint, ?bigint, ?text, ?text, ?text)",
                &[Sql::Int(seq), Sql::Int(t as i64), Sql::Text(e), Sql::Text(old), Sql::Text(new)],
            )?;
        }
        Ok(())
    }

    // ---- migration --------------------------------------------------------------------

    /// Tables added and removed compared to the schema stored in the database.
    pub fn plan(&mut self) -> Result<(Vec<String>, Vec<String>), String> {
        let stored: Vec<String> = self.meta("schema_tables")?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let current: Vec<String> = self.schema.tables.iter().map(|t| t.name.clone()).collect();
        let added = current.iter().filter(|t| !stored.contains(t)).cloned().collect();
        let removed = stored.iter().filter(|t| !current.contains(t)).cloned().collect();
        Ok((added, removed))
    }

    /// Replace every projection table with the current schema and fill it from `after`,
    /// recording the migration transaction (`after.tx`) in the same database transaction.
    pub fn apply_migration(&mut self, model: &Model, before: &Store, after: &Store, cause: &str, time: chrono::NaiveDateTime) -> Result<(), String> {
        let stored: Vec<String> = self.meta("schema_tables")?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        self.conn.batch("BEGIN")?;
        let r = (|| -> Result<(), String> {
            let cascade = if self.dialect() == Dialect::Postgres { " CASCADE" } else { "" };
            for t in stored.iter().rev() {
                self.conn.batch(&format!("DROP TABLE IF EXISTS {}{cascade}", q(t)))?;
            }
            for s in self.schema.ddl(self.dialect()) {
                self.conn.batch(&s)?;
            }
            self.sync_indexes(None, false)?;
            let ids: Vec<u64> = after.sorted().iter().map(|r| r.id).collect();
            for id in &ids {
                if let Some(r) = after.records.get(id) {
                    self.record_meta(r, r.version().max(after.tx), r.version())?;
                }
                self.project_record(model, after, *id, &|t| t)?;
            }
            for row in &after.rows {
                self.project_row(row)?;
            }
            let commit = Commit { tx: after.tx, base: before.tx, before, after, user: None, cause: cause.to_string(), time };
            if after.tx > before.tx {
                // the migration's own writes (defaults of new fields) go through the log
                let mut only_new = before.clone();
                only_new.materialized = after.materialized.clone();
                let commit = Commit { before: &only_new, ..commit };
                self.write_tx_without_projections(&commit)?;
            }
            let fp = self.schema.fingerprint();
            self.set_meta("schema_fingerprint", &fp)?;
            let names: Vec<String> = self.schema.tables.iter().map(|t| t.name.clone()).collect();
            self.set_meta("schema_tables", &serde_json::to_string(&names).unwrap())?;
            Ok(())
        })();
        self.finish(r)
    }

    fn write_tx_without_projections(&mut self, c: &Commit) -> Result<(), String> {
        let tx = c.tx;
        let mut payload = Vec::new();
        for r in c.after.sorted() {
            for (t, f, v) in &r.history {
                if *t == tx {
                    self.seq += 1;
                    let value = to_json(v).to_string();
                    payload.push(json!(["h", r.id, f, value]));
                    self.conn.exec(
                        "INSERT INTO \"_quadra_history\" (\"seq\", \"tx\", \"record_id\", \"field\", \"value\") VALUES (?bigint, ?bigint, ?bigint, ?text, ?text)",
                        &[Sql::Int(self.seq), Sql::Int(tx as i64), Sql::Int(r.id as i64), Sql::Text(f.clone()), Sql::Text(value)],
                    )?;
                }
            }
        }
        self.write_scripts(c, &mut payload, &|t| t)?;
        self.write_outbox(c, tx)?;
        let state = json!({ "next_id": c.after.next_id, "tx": c.after.tx }).to_string();
        self.set_meta("state", &state)?;
        let at = c.time.format("%Y-%m-%dT%H:%M:%S").to_string();
        let hash = commit_hash(&self.prev_hash, tx, &at, c.user, &c.cause, &payload);
        self.conn.exec(
            "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text)",
            &[Sql::Int(tx as i64), Sql::Text(at), Sql::Null, Sql::Text(c.cause.clone()), Sql::Text(self.prev_hash.clone()), Sql::Text(hash.clone())],
        )?;
        self.prev_hash = hash;
        Ok(())
    }

    // ---- verification -----------------------------------------------------------------

    /// Check the hash chain and that projections match the truth (`store` loaded from this
    /// database, with materialized formulas computed). Returns the problems found.
    pub fn verify(&mut self, model: &Model, store: &Store) -> Result<Vec<String>, String> {
        let mut problems = Vec::new();
        // 1. hash chain
        let mut by_tx: HashMap<i64, Vec<J>> = HashMap::new();
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\" FROM \"_quadra_history\" ORDER BY \"_quadra_history\".\"seq\"", &[])? {
            by_tx.entry(num(&r[0])).or_default().push(json!(["h", num(&r[1]), r[2].clone().unwrap_or_default(), r[3].clone().unwrap_or_default()]));
        }
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), CAST(\"id\" AS TEXT), \"ledger\", \"cols\" FROM \"_quadra_ledger_rows\" ORDER BY \"_quadra_ledger_rows\".\"id\"", &[])? {
            by_tx.entry(num(&r[0])).or_default().push(json!(["r", num(&r[1]), r[2].clone().unwrap_or_default(), r[3].clone().unwrap_or_default()]));
        }
        let mut applied: Vec<(i64, String)> = Vec::new();
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), \"key\" FROM \"_quadra_applied\"", &[])? {
            applied.push((num(&r[0]), r[1].clone().unwrap_or_default()));
        }
        applied.sort();
        for (tx, k) in applied {
            by_tx.entry(tx).or_default().push(json!(["a", k]));
        }
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), \"entity\", \"old_name\", \"new_name\" FROM \"_quadra_renames\" ORDER BY \"_quadra_renames\".\"seq\"", &[])? {
            by_tx.entry(num(&r[0])).or_default().push(json!(["n", r[1], r[2], r[3]]));
        }
        let mut prev = "0".repeat(64);
        for r in self.conn.query("SELECT CAST(\"tx\" AS TEXT), \"at\", CAST(\"user_id\" AS TEXT), \"cause\", \"prev_hash\", \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\"", &[])? {
            let tx = num(&r[0]);
            if r[4].as_deref() != Some(prev.as_str()) {
                problems.push(format!("commit {tx}: previous hash does not match the chain"));
            }
            let user = r[2].as_deref().and_then(|u| u.parse().ok());
            let payload = by_tx.remove(&tx).unwrap_or_default();
            let h = commit_hash(&prev, tx as u64, r[1].as_deref().unwrap_or(""), user, r[3].as_deref().unwrap_or(""), &payload);
            if r[5].as_deref() != Some(h.as_str()) {
                problems.push(format!("commit {tx}: content does not match its hash (history or ledger rows were altered)"));
            }
            prev = r[5].clone().unwrap_or_default();
        }
        for tx in by_tx.keys() {
            problems.push(format!("writes of transaction {tx} have no commit row"));
        }
        // 2. projections against the truth
        let tables: Vec<Table> = self.schema.tables.iter().filter(|t| matches!(t.source, Source::Entity(_))).cloned().collect();
        for t in tables {
            let Source::Entity(ent) = &t.source else {
                continue;
            };
            let mut expected: HashMap<u64, Vec<Option<String>>> = HashMap::new();
            for id in store.by_entity.get(ent).cloned().unwrap_or_default() {
                let rec = &store.records[&id];
                let mut row = Vec::new();
                for fm in &t.fields {
                    let v = Self::field_value(store, rec, fm);
                    for c in &fm.columns {
                        row.push(self.encode(c, &v).as_text());
                    }
                }
                expected.insert(id, row);
            }
            let cols: Vec<String> = t.fields.iter().flat_map(|f| f.columns.iter().map(|c| format!("CAST({} AS TEXT)", q(&c.name)))).collect();
            let names: Vec<String> = t.fields.iter().flat_map(|f| f.columns.iter().map(|c| c.name.clone())).collect();
            let sql = format!("SELECT CAST(\"id\" AS TEXT){} FROM {}", cols.iter().map(|c| format!(", {c}")).collect::<String>(), q(&t.name));
            let rows = self.conn.query(&sql, &[])?;
            if rows.len() != expected.len() {
                problems.push(format!("table {}: {} rows, the history has {}", t.name, rows.len(), expected.len()));
            }
            for r in rows {
                let id = num(&r[0]) as u64;
                let Some(exp) = expected.get(&id) else {
                    problems.push(format!("table {}: row {id} is not in the history", t.name));
                    continue;
                };
                for (i, name) in names.iter().enumerate() {
                    if !same(&r[i + 1], &exp[i]) {
                        problems.push(format!("{}.{} of {id}: the table has {}, the history says {}", t.name, name, r[i + 1].as_deref().unwrap_or("NULL"), exp[i].as_deref().unwrap_or("NULL")));
                    }
                }
            }
        }
        let _ = model;
        Ok(problems)
    }
}

/// Whether a record's projection row reads back exactly as its fields (`source::decode`):
/// each stored value is of the kind its columns say (a decimal in a `Decimal` field, not an
/// integer; a percentage in a `Percent` field…), a set of references in increasing id order
/// (its join table keeps no order, `fits_refs`), a per-company value of a company, no
/// provisional marker. Otherwise `_quadra_records.version_tx` is 0 and the record is read
/// from its history.
pub(crate) fn exact_row(schema: &Schema, rec: &Record) -> bool {
    exact_values(schema, &rec.ent, rec.fields.iter().map(|(k, v)| (k.as_str(), v)))
}

/// `exact_row` of the stored values of a record of `ent`.
pub(crate) fn exact_values<'a>(schema: &Schema, ent: &str, fields: impl Iterator<Item = (&'a str, &'a V)>) -> bool {
    if !schema.readable.contains(ent) {
        return false;
    }
    let Some(t) = schema.entity_table(ent) else {
        return false;
    };
    let sets = schema.sets.get(ent);
    for (k, v) in fields {
        if k.contains('#') {
            return false;
        }
        if let Some((f, company)) = k.split_once('@') {
            // a per-company value: a row of the field's table (company 0 has none)
            let Some(pt) = schema.per_company_table(ent, f) else {
                return false;
            };
            if company.parse::<u64>().map_or(true, |c| c == 0) || !pt.fields.first().is_some_and(|fm| fits(&fm.decode, v) || kind_note(&fm.decode, v).is_some()) {
                return false;
            }
            continue;
        }
        if let Some(sm) = sets.and_then(|s| s.iter().find(|m| m.field == k)) {
            if !fits_refs(sm, v) {
                return false;
            }
            continue;
        }
        let Some(fm) = t.fields.iter().find(|f| f.field == k && !f.formula) else {
            continue;
        };
        if !fits(&fm.decode, v) && kind_note(&fm.decode, v).is_none() {
            return false;
        }
    }
    true
}

/// A number held by columns of another kind of number (an integer in a `Decimal` field, a
/// decimal in a `Percent` field, a plain number in an amount's): what it is, to read it back
/// as itself (`_quadra_records.kinds`, `source::decode_kind`).
pub(crate) fn kind_note(d: &crate::schema::Decode, v: &V) -> Option<&'static str> {
    use crate::schema::Decode;
    let numeric = matches!(d, Decode::Dec | Decode::Pct | Decode::Money | Decode::Price | Decode::Qty);
    match v {
        V::Int(_) if numeric => Some("int"),
        V::Dec(_) if numeric && *d != Decode::Dec => Some("dec"),
        V::Pct(_) if numeric && *d != Decode::Pct => Some("pct"),
        _ => None,
    }
}

/// The `kinds` of a record's row: its stored values of another kind of number than their
/// columns say, as a JSON object (none: NULL).
pub(crate) fn kinds_of(schema: &Schema, rec: &Record) -> Option<String> {
    let t = schema.entity_table(&rec.ent)?;
    let mut out = serde_json::Map::new();
    for (k, v) in &rec.fields {
        let fm = match k.split_once('@') {
            Some((f, _)) => schema.per_company_table(&rec.ent, f).and_then(|pt| pt.fields.first()),
            None => t.fields.iter().find(|f| f.field == *k && !f.formula),
        };
        if let Some(n) = fm.and_then(|fm| kind_note(&fm.decode, v)) {
            out.insert(k.clone(), J::String(n.to_string()));
        }
    }
    (!out.is_empty()).then(|| J::Object(out).to_string())
}

/// Whether a set (or list) of references reads back from its join table as itself: the set
/// or list its type says, of references to its entity, each once (a row per reference, in
/// the order of `ord`). No value at all reads back as the empty set (`Interp::get_ref` gives
/// it).
pub(crate) fn fits_refs(sm: &crate::schema::SetMap, v: &V) -> bool {
    let items = match (sm.set, v) {
        (true, V::Set(items)) | (false, V::List(items)) => items,
        _ => return false,
    };
    let mut seen = HashSet::new();
    items.iter().all(|it| matches!(it, V::Ref { ent, id } if **ent == *sm.target && seen.insert(*id)))
}

/// Whether a value reads back from columns of this kind as itself (`exact_row`).
pub(crate) fn fits(d: &crate::schema::Decode, v: &V) -> bool {
    use crate::schema::Decode;
    use chrono::Timelike;
    {
        let fits = match (d, v) {
            (_, V::None) => true,
            (Decode::Bool, V::Bool(_))
            | (Decode::Int, V::Int(_))
            | (Decode::Dec, V::Dec(_))
            | (Decode::Pct, V::Pct(_))
            | (Decode::Text, V::Text(_))
            | (Decode::Role, V::Role(_))
            | (Decode::Date, V::Date(_))
            | (Decode::Duration, V::Duration(_))
            | (Decode::Money, V::Money { .. })
            | (Decode::Price, V::Price { .. })
            | (Decode::Qty, V::Qty { .. })
            | (Decode::YearMonth, V::YearMonth(..))
            | (Decode::Json, _) => true,
            (Decode::Variant, V::Variant { fields, .. }) => fields.is_empty(),
            (Decode::DateTime, V::DateTime(d)) => d.nanosecond() == 0,
            (Decode::Ref(e), V::Ref { ent, .. }) => **ent == **e,
            _ => false,
        };
        fits
    }
}

/// The SQL value of one column of a field (`Column::part` says which part of the value).
pub(crate) fn encode(c: &Column, v: &V) -> Sql {
    match (c.part, v) {
        (_, V::None) => Sql::Null,
        (Part::Json, v) => Sql::Text(to_json(v).to_string()),
        (Part::RefId, V::Ref { id, .. }) => Sql::Int(*id as i64),
        (Part::Amount, V::Money { amt, .. } | V::Price { amt, .. }) => dec_text(amt),
        (Part::Amount, V::Qty { v, .. }) => dec_text(v),
        (Part::Amount, x) => x.num().map(|n| dec_text(&n)).unwrap_or(Sql::Null),
        (Part::Currency, V::Money { cur, .. } | V::Price { cur, .. }) => Sql::Text(cur.to_string()),
        (Part::Per, V::Price { per, .. }) => Sql::Text(per.to_string()),
        (Part::Unit, V::Qty { unit, .. }) => Sql::Text(unit.to_string()),
        (Part::Duration, V::Duration(s)) => Sql::Int(*s),
        (Part::Scalar, V::Bool(b)) => Sql::Bool(*b),
        (Part::Scalar, V::Int(i)) if c.ty == ColType::BigInt => Sql::Int(*i),
        (Part::Scalar, x @ (V::Int(_) | V::Dec(_) | V::Pct(_))) => x.num().map(|n| dec_text(&n)).unwrap_or(Sql::Null),
        (Part::Scalar, x @ (V::Date(_) | V::DateTime(_))) => date_text(x),
        (Part::Scalar, V::Variant { name, .. }) | (Part::Scalar, V::Role(name)) => Sql::Text(name.to_string()),
        (Part::Scalar, V::Text(t)) => Sql::Text(t.to_string()),
        (Part::Scalar, V::YearMonth(y, m)) => Sql::Text(format!("{y}-{m:02}")),
        _ => Sql::Null,
    }
}

/// Compare a value read back from the database with the expected text.
pub(crate) fn same(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            if x == y {
                return true;
            }
            let norm_bool = |s: &str| match s {
                "1" | "t" | "true" => Some(true),
                "0" | "f" | "false" => Some(false),
                _ => None,
            };
            if let (Some(p), Some(q)) = (norm_bool(x), norm_bool(y)) {
                return p == q;
            }
            if let (Ok(p), Ok(q)) = (x.parse::<rust_decimal::Decimal>(), y.parse::<rust_decimal::Decimal>()) {
                return p == q;
            }
            if let (Ok(p), Ok(q)) = (serde_json::from_str::<J>(x), serde_json::from_str::<J>(y)) {
                return p == q;
            }
            false
        }
        _ => false,
    }
}

/// Applied scripts and renames of a transaction: written next to the history, part of the hash.
fn script_payload(before: &Store, after: &Store) -> (Vec<(String, u64)>, Vec<(String, String, String, u64)>) {
    let mut applied: Vec<(String, u64)> = after.applied.iter().filter(|(k, _)| !before.applied.contains_key(*k)).map(|(k, t)| (k.clone(), *t)).collect();
    applied.sort();
    let renames = after.renames.iter().skip(before.renames.len()).cloned().collect();
    (applied, renames)
}

fn commit_hash(prev: &str, tx: u64, at: &str, user: Option<u64>, cause: &str, payload: &[J]) -> String {
    let mut h = CommitHasher::new(prev, tx, at, user, cause);
    for p in payload {
        h.entry(&p.to_string());
    }
    h.finish()
}

/// The hash of a commit computed entry by entry: the bulk import streams millions of
/// history entries and never holds a commit's payload in memory.
pub(crate) struct CommitHasher(Sha256);

impl CommitHasher {
    pub(crate) fn new(prev: &str, tx: u64, at: &str, user: Option<u64>, cause: &str) -> CommitHasher {
        let mut h = Sha256::new();
        h.update(prev.as_bytes());
        h.update(format!("|{tx}|{at}|{}|{cause}|", user.map(|u| u.to_string()).unwrap_or_default()).as_bytes());
        CommitHasher(h)
    }

    /// One payload entry, as the compact JSON text of the array (`["h",id,"field","value"]`).
    pub(crate) fn entry(&mut self, json: &str) {
        self.0.update(json.as_bytes());
        self.0.update(b"\n");
    }

    pub(crate) fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

/// The engine writes through this sink: one database transaction per engine transaction.
pub struct Sink<'m> {
    pub db: Database,
    pub model: &'m Model,
}

impl<'m> CommitSink for Sink<'m> {
    /// One database transaction: lock the commit log, refuse when others changed what this
    /// transaction read after its base, write under the next transaction number.
    fn commit(&mut self, c: &Commit) -> Result<u64, CommitError> {
        let db = &mut self.db;
        let begin = if db.dialect() == Dialect::Sqlite { "BEGIN IMMEDIATE" } else { "BEGIN" };
        db.conn.batch(begin).map_err(CommitError::Failed)?;
        let model = self.model;
        let r = (|| -> Result<u64, CommitError> {
            let f = CommitError::Failed;
            if db.dialect() == Dialect::Postgres {
                db.conn.batch("LOCK TABLE \"_quadra_commits\" IN EXCLUSIVE MODE").map_err(f)?;
            }
            let head = crate::source::head(&mut db.conn).map_err(f)?;
            if head > c.base {
                let conflicts = if c.after.lazy() { db.find_conflicts(c.base, &c.after.reads) } else { db.commits_after(c.base) }.map_err(f)?;
                if !conflicts.is_empty() {
                    return Err(CommitError::Conflict(conflicts));
                }
            }
            // sequence, chain and ids from the database (another process may have written), in
            // one round trip
            let r = db
                .conn
                .query(
                    "SELECT CAST((SELECT MAX(\"seq\") FROM \"_quadra_history\") AS TEXT), (SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1), (SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'state')",
                    &[],
                )
                .map_err(f)?;
            let row = r.into_iter().next().unwrap_or_default();
            db.seq = num(&row.first().cloned().flatten());
            db.prev_hash = row.get(1).cloned().flatten().unwrap_or_else(|| "0".repeat(64));
            let stored_next = row.get(2).cloned().flatten().and_then(|s| serde_json::from_str::<J>(&s).ok()).and_then(|s| s["next_id"].as_u64()).unwrap_or(0);
            let tx = head + 1;
            db.write_tx(model, c, tx, stored_next).map_err(f)?;
            Ok(tx)
        })();
        match r {
            Ok(tx) => {
                db.conn.batch("COMMIT").map_err(CommitError::Failed)?;
                Ok(tx)
            }
            Err(e) => {
                let _ = db.conn.batch("ROLLBACK");
                Err(e)
            }
        }
    }
}
