//! A small connection layer over SQLite and PostgreSQL.
//!
//! Generated SQL uses typed placeholders: `?bigint`, `?text`, `?numeric`, `?boolean`,
//! `?date`, `?timestamp`, `?jsonb`. On PostgreSQL they become `$n::text::<type>` and every
//! value is sent as text, so NULLs and decimals need no special handling; on SQLite they
//! become `?` and values are sent natively. Reads always `CAST(... AS TEXT)`.

use quadra_runtime::limits;
use std::sync::atomic::{AtomicU64, Ordering};

/// Statements sent to a database by this process (benchmarks report them per scenario).
pub static STATEMENTS: AtomicU64 = AtomicU64::new(0);

/// Statements sent so far.
pub fn statements() -> u64 {
    STATEMENTS.load(Ordering::Relaxed)
}

/// Time spent in the database by this process, in microseconds (statements sent and rows
/// read back): the benchmarks split a request's time between the database and the engine.
pub static DB_MICROS: AtomicU64 = AtomicU64::new(0);

pub fn db_micros() -> u64 {
    DB_MICROS.load(Ordering::Relaxed)
}

struct Timed(std::time::Instant, Option<String>);

impl Drop for Timed {
    fn drop(&mut self) {
        let us = self.0.elapsed().as_micros() as u64;
        DB_MICROS.fetch_add(us, Ordering::Relaxed);
        if let (Some(sql), Some(limit)) = (&self.1, slow_limit()) {
            if us >= limit {
                eprintln!(
                    "slow statement: {:.1} ms: {}",
                    us as f64 / 1000.0,
                    sql.chars().take(300).collect::<String>()
                );
            }
        }
    }
}

/// `QUADRA_SLOW_SQL=<ms>`: statements that take longer are printed to stderr (diagnostics).
fn slow_limit() -> Option<u64> {
    static LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("QUADRA_SLOW_SQL")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|ms| (ms * 1000.0) as u64)
    })
}

fn timed(sql: &str) -> Timed {
    Timed(
        std::time::Instant::now(),
        slow_limit().map(|_| sql.to_string()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Sql {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

impl Sql {
    pub fn as_text(&self) -> Option<String> {
        match self {
            Sql::Null => None,
            Sql::Int(i) => Some(i.to_string()),
            Sql::Text(t) => Some(t.clone()),
            Sql::Bool(b) => Some(b.to_string()),
        }
    }
}

/// PostgreSQL errors print as "db error" unless the server's message is pulled out. A
/// statement cancelled by `statement_timeout` is the job's time limit.
fn pg(e: postgres::Error) -> String {
    if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) {
        limits::hit(limits::TIMEOUT);
        return limits::TIMEOUT.to_string();
    }
    match e.as_db_error() {
        Some(d) => match d.detail() {
            Some(detail) => format!("{}: {} ({detail})", d.severity(), d.message()),
            None => format!("{}: {}", d.severity(), d.message()),
        },
        None => e.to_string(),
    }
}

/// SQLite: a statement interrupted by the progress handler is the job's time limit; a
/// full database under a size limit is that limit.
fn lite(e: rusqlite::Error) -> String {
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        if f.code == rusqlite::ErrorCode::OperationInterrupted {
            limits::hit(limits::TIMEOUT);
            return limits::TIMEOUT.to_string();
        }
        if f.code == rusqlite::ErrorCode::DiskFull {
            if let Some(max) = MAX_BYTES.with(|m| m.get()) {
                let size = if max >= 1 << 20 {
                    format!("{} MB", max >> 20)
                } else {
                    format!("{} kB", max >> 10)
                };
                let m = format!("too large: the database reached its size limit of {size} (nothing of this request was written)");
                limits::hit(m.clone());
                return m;
            }
        }
    }
    e.to_string()
}

/// The row cap of the job, hit by a statement that returned more rows.
fn too_many(max: usize) -> String {
    let m = limits::too_many_rows(max);
    limits::hit(m.clone());
    m
}

thread_local! {
    /// PostgreSQL: `statement_timeout` of the connections opened on this thread
    static STATEMENT_TIMEOUT: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// SQLite: the size the database files opened on this thread may grow to
    static MAX_BYTES: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// SQLite databases opened from now on by this thread may not grow past `max` bytes (an
/// engine sets it to the limit of its project): a write that needs more fails, and nothing
/// of it is written. A database already larger cannot grow, and can still shrink.
pub fn set_max_bytes(max: Option<u64>) {
    MAX_BYTES.with(|m| m.set(max));
}

/// PostgreSQL connections opened from now on by this thread stop any statement that runs
/// longer than `limit` (an engine sets it to the time limit of its jobs). SQLite needs no
/// setting: its statements stop at the job's deadline (limits.rs).
pub fn set_statement_timeout(limit: Option<std::time::Duration>) {
    STATEMENT_TIMEOUT.with(|t| t.set(limit));
}

pub enum Conn {
    Sqlite(rusqlite::Connection),
    Postgres(postgres::Client),
}

const KINDS: &[&str] = &[
    "bigint",
    "text",
    "numeric",
    "boolean",
    "date",
    "timestamp",
    "jsonb",
];

impl Conn {
    /// `sqlite:path/to.db`, `sqlite::memory:`, `postgres://user@host:port/db`
    pub fn open(url: &str) -> Result<Conn, String> {
        if let Some(path) = url.strip_prefix("sqlite:") {
            let c = if path == ":memory:" {
                rusqlite::Connection::open_in_memory()
            } else {
                rusqlite::Connection::open(path)
            }
            .map_err(|e| e.to_string())?;
            // another connection's checkpoint or write holds the file a moment: wait for it
            // rather than fail at once ("database is locked"). What is deleted is overwritten
            // with zeros: an erased value leaves no trace in the file's free pages
            c.execute_batch(
                "PRAGMA busy_timeout = 5000; PRAGMA secure_delete = ON; PRAGMA foreign_keys = ON;",
            )
            .map_err(|e| e.to_string())?;
            // a new file: the pages freed can go back to the file system (`reclaim`). Only
            // before its first page is written; its copies keep it
            let pages: i64 = c
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if pages == 0 {
                c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL")
                    .map_err(|e| e.to_string())?;
            }
            c.execute_batch("PRAGMA journal_mode = WAL;")
                .map_err(|e| e.to_string())?;
            // the file mapped in memory: every connection (every engine of a project) reads the
            // same pages from the system's cache instead of copying them into its own
            if path != ":memory:" {
                c.execute_batch("PRAGMA mmap_size = 1073741824")
                    .map_err(|e| e.to_string())?;
            }
            if let Some(max) = MAX_BYTES.with(|m| m.get()) {
                let page: u64 = c
                    .query_row("PRAGMA page_size", [], |r| r.get(0))
                    .map_err(|e| e.to_string())?;
                // a database already larger keeps its size: SQLite never lowers the count below it
                c.execute_batch(&format!(
                    "PRAGMA max_page_count = {}",
                    (max / page.max(1)).max(1)
                ))
                .map_err(|e| e.to_string())?;
            }
            c.set_prepared_statement_cache_capacity(256);
            sqlite_functions(&c).map_err(|e| e.to_string())?;
            // every few thousand steps of the virtual machine: past the deadline of the job
            // running on this thread, the statement stops
            c.progress_handler(10_000, Some(limits::expired));
            Ok(Conn::Sqlite(c))
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let (base, schema) = match url.split_once("#schema=") {
                Some((b, s)) => (b, Some(s)),
                None => (url, None),
            };
            let mut c = postgres::Client::connect(base, postgres::NoTls).map_err(pg)?;
            c.batch_execute("SET client_min_messages = warning")
                .map_err(pg)?;
            if let Some(t) = STATEMENT_TIMEOUT.with(|t| t.get()) {
                c.batch_execute(&format!("SET statement_timeout = {}", t.as_millis().max(1)))
                    .map_err(pg)?;
            }
            if let Some(s) = schema {
                c.batch_execute(&format!(
                    "CREATE SCHEMA IF NOT EXISTS \"{s}\"; SET search_path TO \"{s}\""
                ))
                .map_err(pg)?;
            }
            Ok(Conn::Postgres(c))
        } else {
            Err(format!(
                "unsupported database url `{url}` (use sqlite:… or postgres://…)"
            ))
        }
    }

    pub fn dialect(&self) -> Dialect {
        match self {
            Conn::Sqlite(_) => Dialect::Sqlite,
            Conn::Postgres(_) => Dialect::Postgres,
        }
    }

    fn rewrite(&self, sql: &str) -> String {
        let mut out = String::with_capacity(sql.len() + 32);
        let mut n = 0;
        let mut rest = sql;
        while let Some(i) = rest.find('?') {
            out.push_str(&rest[..i]);
            rest = &rest[i + 1..];
            let kind = KINDS
                .iter()
                .find(|k| rest.starts_with(**k))
                .copied()
                .unwrap_or("text");
            if rest.starts_with(kind) {
                rest = &rest[kind.len()..];
            }
            n += 1;
            match self {
                Conn::Sqlite(_) => out.push('?'),
                Conn::Postgres(_) => out.push_str(&format!("${n}::text::{kind}")),
            }
        }
        out.push_str(rest);
        out
    }

    pub fn batch(&mut self, sql: &str) -> Result<(), String> {
        let sql = match side_batch(sql) {
            SideBatch::Run(rest) => rest,
            SideBatch::Skip => return Ok(()),
        };
        let sql = sql.as_str();
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        let _t = timed(sql);
        match self {
            Conn::Sqlite(c) => c
                .execute_batch(sql)
                .map_err(|e| format!("{}\n{sql}", lite(e))),
            Conn::Postgres(c) => c
                .batch_execute(sql)
                .map_err(|e| format!("{}\n{sql}", pg(e))),
        }
    }

    pub fn exec(&mut self, sql: &str, params: &[Sql]) -> Result<u64, String> {
        let r = self.exec_counted(sql, params);
        if let Ok(n) = &r {
            limits::count_written(*n);
            side_record(sql, params);
        }
        r
    }

    fn exec_counted(&mut self, sql: &str, params: &[Sql]) -> Result<u64, String> {
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        let _t = timed(sql);
        let sql = self.rewrite(sql);
        match self {
            Conn::Sqlite(c) => {
                let ps: Vec<rusqlite::types::Value> = params.iter().map(sqlite_value).collect();
                let mut stmt = c.prepare_cached(&sql).map_err(|e| format!("{e}\n{sql}"))?;
                stmt.execute(rusqlite::params_from_iter(ps))
                    .map(|n| n as u64)
                    .map_err(|e| format!("{}\n{sql}", lite(e)))
            }
            Conn::Postgres(c) => {
                // parameters are text (`$n::text::<type>`): typed, the statement takes one round
                // trip instead of three (prepare, execute, close)
                let texts: Vec<Option<String>> = params.iter().map(Sql::as_text).collect();
                let typed: Vec<(&(dyn postgres::types::ToSql + Sync), postgres::types::Type)> =
                    texts
                        .iter()
                        .map(|t| {
                            (
                                t as &(dyn postgres::types::ToSql + Sync),
                                postgres::types::Type::TEXT,
                            )
                        })
                        .collect();
                c.execute_typed(sql.as_str(), &typed)
                    .map_err(|e| format!("{}\n{sql}", pg(e)))
            }
        }
    }

    /// Rows of text values (`CAST(col AS TEXT)` in the query).
    pub fn query(&mut self, sql: &str, params: &[Sql]) -> Result<Vec<Vec<Option<String>>>, String> {
        let r = self.query_counted(sql, params);
        if let Ok(rows) = &r {
            limits::count_read(rows.len() as u64);
        }
        r
    }

    fn query_counted(
        &mut self,
        sql: &str,
        params: &[Sql],
    ) -> Result<Vec<Vec<Option<String>>>, String> {
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        let _t = timed(sql);
        let sql = self.rewrite(sql);
        match self {
            Conn::Sqlite(c) => {
                let ps: Vec<rusqlite::types::Value> = params.iter().map(sqlite_value).collect();
                let mut stmt = c.prepare_cached(&sql).map_err(|e| format!("{e}\n{sql}"))?;
                let n = stmt.column_count();
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(ps), |row| {
                        (0..n)
                            .map(|i| {
                                row.get::<_, rusqlite::types::Value>(i).map(|v| match v {
                                    rusqlite::types::Value::Null => None,
                                    rusqlite::types::Value::Integer(i) => Some(i.to_string()),
                                    rusqlite::types::Value::Real(r) => Some(r.to_string()),
                                    rusqlite::types::Value::Text(t) => Some(t),
                                    rusqlite::types::Value::Blob(_) => None,
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .map_err(lite)?;
                // read one row past the cap at most: a statement that returns too many stops there
                let max = limits::current().max_rows;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(lite)?);
                    if let Some(max) = max.filter(|m| out.len() > *m) {
                        return Err(too_many(max));
                    }
                }
                Ok(out)
            }
            Conn::Postgres(c) => {
                let texts: Vec<Option<String>> = params.iter().map(Sql::as_text).collect();
                let typed: Vec<(&(dyn postgres::types::ToSql + Sync), postgres::types::Type)> =
                    texts
                        .iter()
                        .map(|t| {
                            (
                                t as &(dyn postgres::types::ToSql + Sync),
                                postgres::types::Type::TEXT,
                            )
                        })
                        .collect();
                let text = |r: &postgres::Row| {
                    (0..r.len())
                        .map(|i| r.get::<_, Option<String>>(i))
                        .collect::<Vec<_>>()
                };
                match limits::current().max_rows {
                    None => {
                        let rows = c
                            .query_typed(sql.as_str(), &typed)
                            .map_err(|e| format!("{}\n{sql}", pg(e)))?;
                        Ok(rows.iter().map(text).collect())
                    }
                    Some(max) => {
                        // streamed: the rows past the cap are never kept
                        use postgres::fallible_iterator::FallibleIterator;
                        let mut it = c
                            .query_typed_raw(sql.as_str(), typed)
                            .map_err(|e| format!("{}\n{sql}", pg(e)))?;
                        let mut out = Vec::new();
                        while let Some(r) = it.next().map_err(|e| format!("{}\n{sql}", pg(e)))? {
                            out.push(text(&r));
                            if out.len() > max {
                                return Err(too_many(max));
                            }
                        }
                        Ok(out)
                    }
                }
            }
        }
    }

    /// Many rows into one table, streamed: a prepared statement inside the caller's
    /// transaction on SQLite, `COPY … FROM STDIN` on PostgreSQL. Each row has one value per
    /// column, as `exec` would send it. Returns the number of rows.
    pub fn bulk_insert(
        &mut self,
        table: &str,
        cols: &[&str],
        rows: &mut dyn Iterator<Item = Vec<Sql>>,
    ) -> Result<u64, String> {
        let names: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
        let mut n = 0u64;
        match self {
            Conn::Sqlite(c) => {
                // several rows per statement (fewer steps of the virtual machine), the rest one by one
                let one = format!("({})", vec!["?"; cols.len()].join(", "));
                let per = (32_000 / cols.len().max(1)).clamp(1, 64);
                let sql = format!(
                    "INSERT INTO \"{table}\" ({}) VALUES {}",
                    names.join(", "),
                    vec![one.as_str(); per].join(", ")
                );
                let single = format!(
                    "INSERT INTO \"{table}\" ({}) VALUES {one}",
                    names.join(", ")
                );
                let mut stmt = c.prepare_cached(&sql).map_err(|e| format!("{e}\n{sql}"))?;
                let mut buf: Vec<Vec<Sql>> = Vec::with_capacity(per);
                for row in rows {
                    buf.push(row);
                    if buf.len() == per {
                        stmt.execute(rusqlite::params_from_iter(
                            buf.iter().flat_map(|r| r.iter().map(SqlRef)),
                        ))
                        .map_err(|e| format!("{table}: {e}"))?;
                        n += per as u64;
                        buf.clear();
                    }
                }
                if !buf.is_empty() {
                    let mut stmt = c
                        .prepare_cached(&single)
                        .map_err(|e| format!("{e}\n{single}"))?;
                    for row in &buf {
                        stmt.execute(rusqlite::params_from_iter(row.iter().map(SqlRef)))
                            .map_err(|e| format!("{table}: {e}"))?;
                        n += 1;
                    }
                }
            }
            Conn::Postgres(c) => {
                use std::io::Write;
                let sql = format!("COPY \"{table}\" ({}) FROM STDIN", names.join(", "));
                let mut w = c
                    .copy_in(sql.as_str())
                    .map_err(|e| format!("{}\n{sql}", pg(e)))?;
                let mut buf = String::with_capacity(1 << 17);
                for row in rows {
                    for (i, v) in row.iter().enumerate() {
                        if i > 0 {
                            buf.push('\t');
                        }
                        match v {
                            Sql::Null => buf.push_str("\\N"),
                            Sql::Int(x) => {
                                use std::fmt::Write as _;
                                let _ = write!(buf, "{x}");
                            }
                            Sql::Bool(b) => buf.push(if *b { 't' } else { 'f' }),
                            Sql::Text(t) => copy_escape(&mut buf, t),
                        }
                    }
                    buf.push('\n');
                    n += 1;
                    if buf.len() >= 1 << 16 {
                        w.write_all(buf.as_bytes())
                            .map_err(|e| format!("{table}: {e}"))?;
                        buf.clear();
                    }
                }
                w.write_all(buf.as_bytes())
                    .map_err(|e| format!("{table}: {e}"))?;
                w.finish().map_err(|e| format!("{table}: {}", pg(e)))?;
            }
        }
        Ok(n)
    }

    /// PostgreSQL: `COPY` of rows `fill` writes as COPY text (tab-separated, escaped with
    /// `copy_escape`, one per line) into the buffer it is given, until it returns false: what
    /// a writer that knows its rows sends without building them as values first.
    pub fn copy_text(
        &mut self,
        table: &str,
        cols: &[&str],
        fill: &mut dyn FnMut(&mut String) -> Result<bool, String>,
    ) -> Result<(), String> {
        let Conn::Postgres(c) = self else {
            return Err("COPY needs PostgreSQL".into());
        };
        use std::io::Write;
        let names: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
        let sql = format!("COPY \"{table}\" ({}) FROM STDIN", names.join(", "));
        let mut w = c
            .copy_in(sql.as_str())
            .map_err(|e| format!("{}\n{sql}", pg(e)))?;
        let mut buf = String::with_capacity(1 << 17);
        loop {
            let more = fill(&mut buf)?;
            if buf.len() >= 1 << 16 || !more {
                w.write_all(buf.as_bytes())
                    .map_err(|e| format!("{table}: {e}"))?;
                buf.clear();
            }
            if !more {
                break;
            }
        }
        w.finish().map_err(|e| format!("{table}: {}", pg(e)))?;
        Ok(())
    }

    /// No transaction left open, the connection alive: it can serve another request.
    pub fn idle(&self) -> bool {
        match self {
            Conn::Sqlite(c) => c.is_autocommit(),
            Conn::Postgres(c) => !c.is_closed(),
        }
    }

    pub fn table_exists(&mut self, name: &str) -> Result<bool, String> {
        let sql = match self.dialect() {
            Dialect::Sqlite => "SELECT CAST(name AS TEXT) FROM sqlite_master WHERE type = 'table' AND name = ?text",
            Dialect::Postgres => "SELECT CAST(table_name AS TEXT) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = ?text",
        };
        Ok(!self.query(sql, &[Sql::Text(name.into())])?.is_empty())
    }
}

/// SQLite stores decimals as text: `quadra_dec` compares them as numbers (exactly, as the
/// interpreter does), `quadra_dsum` adds them up exactly.
fn sqlite_functions(c: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    c.create_collation("quadra_dec", |a: &str, b: &str| {
        match (Decimal::from_str(a), Decimal::from_str(b)) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        }
    })?;
    struct Sum;
    impl rusqlite::functions::Aggregate<Option<Decimal>, Option<String>> for Sum {
        fn init(
            &self,
            _: &mut rusqlite::functions::Context<'_>,
        ) -> rusqlite::Result<Option<Decimal>> {
            Ok(None)
        }
        fn step(
            &self,
            ctx: &mut rusqlite::functions::Context<'_>,
            acc: &mut Option<Decimal>,
        ) -> rusqlite::Result<()> {
            let v: Option<String> = match ctx.get_raw(0) {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(i) => Some(i.to_string()),
                rusqlite::types::ValueRef::Real(r) => Some(r.to_string()),
                rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                rusqlite::types::ValueRef::Blob(_) => None,
            };
            if let Some(d) = v.and_then(|s| Decimal::from_str(&s).ok()) {
                *acc = Some(acc.map_or(d, |a| a + d));
            }
            Ok(())
        }
        fn finalize(
            &self,
            _: &mut rusqlite::functions::Context<'_>,
            acc: Option<Option<Decimal>>,
        ) -> rusqlite::Result<Option<String>> {
            Ok(acc.flatten().map(|d| d.to_string()))
        }
    }
    c.create_aggregate_function(
        "quadra_dsum",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        Sum,
    )?;
    // the words of a text as the full-text search indexes them (`query::words`), separated by
    // spaces: what the FTS5 tables of the search are given
    c.create_scalar_function(
        "quadra_words",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            Ok(match ctx.get_raw(0) {
                rusqlite::types::ValueRef::Text(t) => {
                    Some(quadra_runtime::query::words(&String::from_utf8_lossy(t)).join(" "))
                }
                _ => None::<String>,
            })
        },
    )?;
    Ok(())
}

/// A value in the text format of `COPY`: backslash escapes for the separators.
pub(crate) fn copy_escape(buf: &mut String, t: &str) {
    // (the text is copied as it is up to the next character to escape)
    let mut start = 0;
    for (i, b) in t.bytes().enumerate() {
        let esc = match b {
            b'\\' => "\\\\",
            b'\t' => "\\t",
            b'\n' => "\\n",
            b'\r' => "\\r",
            _ => continue,
        };
        buf.push_str(&t[start..i]);
        buf.push_str(esc);
        start = i + 1;
    }
    buf.push_str(&t[start..]);
}

/// A parameter bound without copying its text.
struct SqlRef<'a>(&'a Sql);

impl rusqlite::ToSql for SqlRef<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(match self.0 {
            Sql::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Sql::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Sql::Text(t) => ToSqlOutput::Borrowed(ValueRef::Text(t.as_bytes())),
            Sql::Bool(b) => ToSqlOutput::Borrowed(ValueRef::Integer(*b as i64)),
        })
    }
}

fn sqlite_value(v: &Sql) -> rusqlite::types::Value {
    match v {
        Sql::Null => rusqlite::types::Value::Null,
        Sql::Int(i) => rusqlite::types::Value::Integer(*i),
        Sql::Text(t) => rusqlite::types::Value::Text(t.clone()),
        Sql::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
    }
}

/// Idle connections of one database, reused by the requests of an engine (one thread).
pub type Idle = std::rc::Rc<std::cell::RefCell<Vec<Conn>>>;

/// How many idle connections a pool keeps.
const POOL_MAX: usize = 8;

/// A connection that goes back to its pool when dropped (if it has one and is idle).
pub struct Pooled {
    conn: Option<Conn>,
    home: Option<Idle>,
    /// inside a read-only transaction (`begin_read`): ended when the connection goes back
    read: bool,
}

impl Pooled {
    /// A connection of its own: closed when dropped.
    pub fn own(conn: Conn) -> Pooled {
        Pooled {
            conn: Some(conn),
            home: None,
            read: false,
        }
    }

    /// The connection, moved out (to a thread of its own): `put_conn` gives it back.
    pub fn take_conn(&mut self) -> Option<Conn> {
        self.conn.take()
    }

    pub fn put_conn(&mut self, c: Conn) {
        self.conn = Some(c);
    }

    /// An idle connection of the pool, or a new one to `url`.
    pub fn take(pool: &Idle, url: &str) -> Result<Pooled, String> {
        let c = pool.borrow_mut().pop();
        let conn = match c {
            Some(c) => c,
            None => Conn::open(url)?,
        };
        Ok(Pooled {
            conn: Some(conn),
            home: Some(pool.clone()),
            read: false,
        })
    }

    /// Every statement from now on sees the database as of one moment: commits made meanwhile
    /// by other connections stay invisible (SQLite: a read transaction of its log; PostgreSQL:
    /// repeatable read). The transaction ends when the connection goes back to its pool.
    pub fn begin_read(&mut self) -> Result<(), String> {
        let sql = match self.dialect() {
            Dialect::Sqlite => "BEGIN",
            Dialect::Postgres => "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
        };
        self.batch(sql)?;
        self.read = true;
        Ok(())
    }

    /// Do not return the connection to its pool (it may be in a bad state).
    pub fn discard(&mut self) {
        self.home = None;
    }
}

impl std::ops::Deref for Pooled {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn.as_ref().expect("connection")
    }
}

impl std::ops::DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("connection")
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if self.read {
            if let Some(c) = self.conn.as_mut() {
                let _ = c.batch("COMMIT");
            }
        }
        if let (Some(c), Some(home)) = (self.conn.take(), self.home.take()) {
            let mut idle = home.borrow_mut();
            if c.idle() && idle.len() < POOL_MAX {
                idle.push(c);
            }
        }
    }
}

// --- changes outside the hash chain, run by the sequencer (`Sequencer::side`) ---------------
//
// While one runs, on the sequencer's thread, what it writes is recorded (the statements of its
// batch, for the destinations) and its own transactions are folded into the sequencer's: a
// `BEGIN` or `COMMIT` does nothing, a `ROLLBACK` marks the change as given up.

/// What a change outside the hash chain wrote.
#[derive(Default)]
pub(crate) struct Side {
    pub statements: Vec<(String, Vec<Sql>)>,
    pub rolled_back: bool,
    /// savepoints open: (name, statements recorded before it)
    savepoints: Vec<(String, usize)>,
}

thread_local! {
    static SIDE: std::cell::RefCell<Option<Side>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn side_begin() {
    SIDE.with(|s| *s.borrow_mut() = Some(Side::default()));
}

pub(crate) fn side_end() -> Side {
    SIDE.with(|s| s.borrow_mut().take()).unwrap_or_default()
}

enum SideBatch {
    Run(String),
    Skip,
}

/// A `batch` during a change outside the hash chain: transaction control folded, the rest run
/// and recorded; what a `ROLLBACK TO` a savepoint undoes is dropped from the record too.
fn side_batch(sql: &str) -> SideBatch {
    SIDE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(side) = s.as_mut() else {
            return SideBatch::Run(sql.to_string());
        };
        let mut rest = Vec::new();
        for part in split_statements(sql) {
            let up = part.to_ascii_uppercase();
            let words: Vec<&str> = up.split_whitespace().collect();
            let name = |w: Option<&&str>| {
                w.map(|n| n.trim_matches('"').to_string())
                    .unwrap_or_default()
            };
            match words.first().copied() {
                Some("BEGIN" | "COMMIT" | "END") => continue,
                Some("ROLLBACK") if words.get(1).is_some_and(|w| *w == "TO") => {
                    // ROLLBACK TO [SAVEPOINT] name: the savepoint stays open
                    let n = name(words.get(if words.get(2) == Some(&"SAVEPOINT") {
                        3
                    } else {
                        2
                    }));
                    if let Some(i) = side.savepoints.iter().rposition(|(s, _)| *s == n) {
                        side.savepoints.truncate(i + 1);
                        side.statements.truncate(side.savepoints[i].1);
                    }
                }
                Some("ROLLBACK") => {
                    side.rolled_back = true;
                    continue;
                }
                Some("SAVEPOINT") => side
                    .savepoints
                    .push((name(words.get(1)), side.statements.len())),
                Some("RELEASE") => {
                    let n = name(words.get(if words.get(1) == Some(&"SAVEPOINT") {
                        2
                    } else {
                        1
                    }));
                    if let Some(i) = side.savepoints.iter().rposition(|(s, _)| *s == n) {
                        side.savepoints.truncate(i);
                    }
                }
                Some("LOCK" | "SELECT") => {}
                _ => side.statements.push((part.to_string(), Vec::new())),
            }
            rest.push(part);
        }
        if rest.is_empty() {
            SideBatch::Skip
        } else {
            SideBatch::Run(rest.join("; "))
        }
    })
}

/// The statements of a batch, split on the `;` outside quotes and comments.
fn split_statements(sql: &str) -> Vec<&str> {
    let b = sql.as_bytes();
    let (mut out, mut start, mut i) = (Vec::new(), 0, 0);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // a doubled quote is the quote itself
                        if b.get(i + 1) == Some(&q) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b';' => {
                out.push(&sql[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&sql[start.min(sql.len())..]);
    out.into_iter()
        .map(without_leading_comments)
        .filter(|p| !p.is_empty())
        .collect()
}

/// A statement without the comments before it (what decides what kind of statement it is).
fn without_leading_comments(mut p: &str) -> &str {
    loop {
        p = p.trim();
        if let Some(r) = p.strip_prefix("--") {
            p = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = p.strip_prefix("/*") {
            p = r.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return p;
        }
    }
}

#[cfg(test)]
mod split_tests {
    #[test]
    fn statements_split_outside_quotes_and_comments() {
        assert_eq!(
            super::split_statements(
                "BEGIN; INSERT INTO t VALUES ('a;b', \"c;\"\"d\"); -- x; y\nCOMMIT;"
            ),
            vec![
                "BEGIN",
                "INSERT INTO t VALUES ('a;b', \"c;\"\"d\")",
                "COMMIT"
            ]
        );
        assert_eq!(
            super::split_statements("UPDATE t SET a = 'it''s; fine' /* ; */; DELETE FROM t"),
            vec!["UPDATE t SET a = 'it''s; fine' /* ; */", "DELETE FROM t"]
        );
    }
}

fn side_record(sql: &str, params: &[Sql]) {
    SIDE.with(|s| {
        if let Some(side) = s.borrow_mut().as_mut() {
            if !sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
                side.statements.push((sql.to_string(), params.to_vec()));
            }
        }
    })
}

/// The space of what was deleted given back to the file system, and the WAL written back into
/// the file and emptied: the old images of the pages an erasure overwrote do not stay in it.
/// A file made before `auto_vacuum` is turned into one with it first (a `VACUUM`, once).
/// `Ok(false)`: a reader held the WAL, so it is not empty yet (try again). Nothing to do on
/// Postgres.
pub fn reclaim(c: &mut Conn) -> Result<bool, String> {
    if c.dialect() != Dialect::Sqlite {
        return Ok(true);
    }
    let first = |c: &mut Conn, q: &str| -> Result<Option<String>, String> {
        Ok(c.query(q, &[])?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone()))
    };
    if first(c, "PRAGMA auto_vacuum")?.as_deref() == Some("0") {
        c.batch("PRAGMA auto_vacuum = INCREMENTAL")?;
        c.batch("VACUUM")?;
    } else {
        // (a row for each page freed: read to the end, or it frees one)
        c.query("PRAGMA incremental_vacuum", &[])?;
    }
    // (`busy` is 1 while a reader holds the WAL)
    Ok(first(c, "PRAGMA wal_checkpoint(TRUNCATE)")?.as_deref() == Some("0"))
}
