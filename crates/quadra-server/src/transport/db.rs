//! The databases of other systems: PostgreSQL, MySQL, SQL Server, and any database with an
//! ODBC driver (DB2 for i on the AS/400, only where the `odbc` feature is built: the agent).
//!
//! **Reading** (`inbound … pull "ORDERS"` with `cursor m.updated_at`): the rows whose cursor
//! column is at or after the saved cursor, in its order, a page at a time:
//! `SELECT * FROM <table> WHERE <column> >= ? ORDER BY <column>` with the dialect's limit
//! (`LIMIT`, `TOP`, `FETCH FIRST`); the first time, the rows whose column is not null. `>=`
//! reads again the rows of the last value, which the inbox drops as duplicates (each row's key
//! is its table, its cursor and the hash of its columns): a row written later with the same
//! value as the last one is not lost, as it would be with `>`. A page whose rows all have the
//! same value could not move on: then all the rows of that value are read (up to
//! [`TIES_MAX`]), and the page after them.
//!
//! **Writing** (`insert("EXCHANGE_ORDERS", ErpOrderRow(…))` in an outbound): one `INSERT` of
//! the record's columns into the table named in the code, and nothing else. A table with a
//! column `quadra_key` gets the event's idempotency key there: a row with that key already
//! there is the event delivered before, and it is not written again.
//!
//! Identifiers are checked (letters, digits, `_`, `$`, a `.` between a schema and a table) and
//! quoted in the dialect; values are always bound as parameters, as text the database reads
//! in the column's type. Values come out as JSON exactly (see [`int`] and the readers of each
//! engine): decimals and integers beyond 2^53 as strings, dates and times as ISO text, binary
//! as base64.

use super::{Ask, Failed, Fetched, Item, Put, Resolved};
use base64::Engine as _;
use quadra_sema::manifest::{ConnectionKind, DbEngine};
use serde_json::{Map, Value as J};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// At most so many rows sharing one value of the cursor are read together.
pub const TIES_MAX: usize = 100_000;

/// Connections kept open between pulls and deliveries, and for how long.
const IDLE_MAX: usize = 16;
const IDLE_FOR: Duration = Duration::from_secs(120);

/// The rows of `table` after the cursor (`Ask::Rows`), each a JSON object of its columns.
pub async fn fetch(c: &Resolved, ask: &Ask, public_only: bool) -> Result<Fetched, Failed> {
    let Ask::Rows {
        table,
        column,
        after,
        limit,
    } = ask
    else {
        return Err(Failed::Never("a database is asked for rows".into()));
    };
    let t = Target::of(c)?;
    let column = column.as_deref().ok_or_else(|| {
        Failed::Never(format!(
            "{}: the cursor of a table is a field of the inbound's record (`cursor m.updated_at`)",
            t.at()
        ))
    })?;
    let q = Query {
        table: quote_path(t.engine, table).map_err(|e| t.never(e))?,
        column: quote(t.engine, column).map_err(|e| t.never(e))?,
        name: column.to_string(),
        limit: (*limit).clamp(1, 10_000) as usize,
    };
    let mut s = Session::open(&t, public_only).await?;
    let r = read(&mut s, &t, &q, after.as_deref()).await;
    s.keep_unless(&t, public_only, &r);
    let rows = r?;
    if let Some(first) = rows.first() {
        if first.cursor.is_none() {
            return Err(t.never(format!(
                "the table {table} has no column `{column}` (its columns: {})",
                first.values.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let cursor = r.cursor.unwrap_or_default();
        items.push(Item {
            name: format!("{table}/{cursor}"),
            body: serde_json::to_vec(&J::Object(r.values)).unwrap_or_default(),
            cursor: Some(cursor),
        });
    }
    Ok(Fetched {
        items,
        settled: Vec::new(),
    })
}

/// A row into the exchange table named in the code (`Put::Row`), once per idempotency key when
/// the table has a column `quadra_key`.
pub async fn put(c: &Resolved, put: &Put, public_only: bool) -> Result<(), Failed> {
    let Put::Row { table, row, key } = put else {
        return Err(Failed::Never("a database takes rows".into()));
    };
    let t = Target::of(c)?;
    let table_q = quote_path(t.engine, table).map_err(|e| t.never(e))?;
    let J::Object(row) = row else {
        return Err(t.never("the row is a record of columns".into()));
    };
    let mut s = Session::open(&t, public_only).await?;
    let r = insert_row(&mut s, &t, table, &table_q, row, key).await;
    s.keep_unless(&t, public_only, &r);
    r
}

/// The rows after `after`: a page, and, when the whole page has one value, all the rows of it
/// and the page after them.
async fn read(
    s: &mut Session,
    t: &Target,
    q: &Query,
    after: Option<&str>,
) -> Result<Vec<Row>, Failed> {
    let w = match after {
        None => Where::Start,
        Some(a) => Where::From(a.to_string()),
    };
    let page = s.select(t, q, &w, q.limit).await?;
    let same =
        page.len() == q.limit && page.first().map(|r| &r.cursor) == page.last().map(|r| &r.cursor);
    let Some(v) = page.first().and_then(|r| r.cursor.clone()).filter(|_| same) else {
        return Ok(page);
    };
    let mut ties = s.select(t, q, &Where::At(v.clone()), TIES_MAX + 1).await?;
    if ties.len() > TIES_MAX {
        return Err(t.never(format!(
            "more than {TIES_MAX} rows have {} = {v}: the cursor needs a column that tells them apart (a growing id, a time of change with fractions of a second)",
            q.name
        )));
    }
    ties.extend(s.select(t, q, &Where::After(v), q.limit).await?);
    Ok(ties)
}

async fn insert_row(
    s: &mut Session,
    t: &Target,
    table: &str,
    table_q: &str,
    row: &Map<String, J>,
    key: &str,
) -> Result<(), Failed> {
    let columns = s.columns(t, table_q).await?;
    let find = |name: &str| {
        columns.iter().find(|c| c.name == name).or_else(|| {
            let mut same = columns.iter().filter(|c| c.name.eq_ignore_ascii_case(name));
            match (same.next(), same.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        })
    };
    let mut names = Vec::new();
    let mut values = Vec::new();
    let mut key_value = None;
    for (k, v) in row {
        let Some(col) = find(k) else {
            return Err(t.never(format!(
                "the table {table} has no column `{k}` (its columns: {})",
                columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let text = text_of(t.engine, v, col.instant);
        if col.name.eq_ignore_ascii_case("quadra_key") {
            key_value = Some((col.name.clone(), text.clone().unwrap_or_default()));
        }
        names.push(quote(t.engine, &col.name).map_err(|e| t.never(e))?);
        values.push(text);
    }
    // the idempotency key, unless the record sets it
    if key_value.is_none() {
        if let Some(col) = find("quadra_key") {
            key_value = Some((col.name.clone(), key.to_string()));
            names.push(quote(t.engine, &col.name).map_err(|e| t.never(e))?);
            values.push(Some(key.to_string()));
        }
    }
    let key_q = match &key_value {
        Some((col, k)) => Some((quote(t.engine, col).map_err(|e| t.never(e))?, k.clone())),
        None => None,
    };
    if let Some((col, k)) = &key_q {
        if s.has(t, table_q, col, k).await? {
            return Ok(());
        }
    }
    match s.insert(t, table_q, &names, &values).await {
        Ok(()) => Ok(()),
        Err(Insert::Failed(f)) => Err(f),
        // a unique key refused it: the same event delivered meanwhile, or another row
        Err(Insert::Duplicate(why)) => match &key_q {
            Some((col, k)) if s.has(t, table_q, col, k).await? => Ok(()),
            _ => Err(t.never(why)),
        },
    }
}

/// A value of a row as the text bound for its column (`None`: NULL).
fn text_of(engine: DbEngine, v: &J, instant: bool) -> Option<String> {
    Some(match v {
        J::Null => return None,
        // 1 and 0: what a boolean column and a number column both read
        J::Bool(b) => if *b { "1" } else { "0" }.into(),
        J::Number(n) => n.to_string(),
        J::String(s) if instant && engine != DbEngine::Postgres => {
            // an instant (`…Z`) into a column without a zone: the same instant in UTC, written
            // as the database reads it
            match chrono::DateTime::parse_from_rfc3339(s) {
                Ok(d) => {
                    let d = d.naive_utc();
                    let sep = if engine == DbEngine::Odbc { " " } else { "T" };
                    format!(
                        "{}{sep}{}{}",
                        d.format("%Y-%m-%d"),
                        d.format("%H:%M:%S"),
                        frac(d.and_utc().timestamp_subsec_nanos())
                    )
                }
                Err(_) => s.clone(),
            }
        }
        J::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Where a connection goes, with the values of its variables here.
struct Target {
    name: String,
    engine: DbEngine,
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    /// ODBC: the connection string, with the user and the password
    odbc: String,
    timeout: Duration,
    insecure: bool,
}

impl Target {
    fn of(c: &Resolved) -> Result<Target, Failed> {
        let ConnectionKind::Database {
            engine,
            url,
            user,
            password,
            timeout_s,
            insecure,
        } = &c.connection.kind
        else {
            return Err(Failed::Never(format!(
                "`{}` is not a database",
                c.connection.name
            )));
        };
        let url = c.setting(url).map_err(Failed::Waiting)?;
        let user = match user {
            Some(u) => c.setting(u).map_err(Failed::Waiting)?,
            None => String::new(),
        };
        let password = match password {
            Some(p) => c.value(p).map_err(Failed::Waiting)?,
            None => String::new(),
        };
        let mut t = Target {
            name: c.connection.name.clone(),
            engine: *engine,
            host: String::new(),
            port: 0,
            database: String::new(),
            user,
            password,
            odbc: String::new(),
            timeout: Duration::from_secs((*timeout_s).max(1)),
            insecure: *insecure,
        };
        if *engine == DbEngine::Odbc {
            let mut s = url.trim().trim_end_matches(';').to_string();
            if !s.contains('=') {
                s = format!("DSN={s}");
            }
            if !t.user.is_empty() {
                s.push_str(&format!(";UID={}", odbc_value(&t.user)));
            }
            if !t.password.is_empty() {
                s.push_str(&format!(";PWD={}", odbc_value(&t.password)));
            }
            t.odbc = s;
            return Ok(t);
        }
        let bad = |why: &str| {
            Failed::Never(format!(
                "`{}`: the url {why} (`{}://host:port/database`)",
                t.name,
                engine_name(*engine)
            ))
        };
        let u = reqwest::Url::parse(&url).map_err(|_| bad("is not a url"))?;
        let schemes: &[&str] = match engine {
            DbEngine::Postgres => &["postgres", "postgresql"],
            DbEngine::Mysql => &["mysql"],
            _ => &["sqlserver"],
        };
        if !schemes.contains(&u.scheme()) {
            return Err(bad(&format!("starts with {}://", schemes[0])));
        }
        if !u.username().is_empty() || u.password().is_some() {
            return Err(bad("has no credentials: they are `user` and `password`"));
        }
        if u.query().is_some() || u.fragment().is_some() {
            return Err(bad("takes no options"));
        }
        t.host = u
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| bad("names a host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        t.port = u.port().unwrap_or(match engine {
            DbEngine::Postgres => 5432,
            DbEngine::Mysql => 3306,
            _ => 1433,
        });
        t.database = u.path().trim_matches('/').to_string();
        if t.database.contains('/') {
            return Err(bad("names one database"));
        }
        Ok(t)
    }

    /// The connection, for messages: never its credentials.
    fn at(&self) -> String {
        if self.engine == DbEngine::Odbc {
            format!("`{}` (odbc)", self.name)
        } else {
            format!(
                "`{}` ({} {}:{})",
                self.name,
                engine_name(self.engine),
                self.host,
                self.port
            )
        }
    }

    fn never(&self, why: String) -> Failed {
        Failed::Never(format!("{}: {why}", self.at()))
    }

    fn again(&self, why: String) -> Failed {
        Failed::Again(format!("{}: {why}", self.at()))
    }

    /// What tells the connections that may be shared apart: where, as whom, how.
    fn key(&self, public_only: bool) -> [u8; 32] {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        for p in [
            engine_name(self.engine),
            &self.host,
            &self.port.to_string(),
            &self.database,
            &self.user,
            &self.password,
            &self.odbc,
            if self.insecure { "insecure" } else { "tls" },
            if public_only { "public" } else { "any" },
        ] {
            h.update(p.as_bytes());
            h.update([0]);
        }
        h.finalize().into()
    }

    /// `fut`, or `Again` when it takes longer than the connection's timeout.
    async fn within<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, Failed>>,
    ) -> Result<T, Failed> {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(self.again(format!("no answer within {} s", self.timeout.as_secs()))),
        }
    }
}

fn engine_name(e: DbEngine) -> &'static str {
    match e {
        DbEngine::Postgres => "postgres",
        DbEngine::Mysql => "mysql",
        DbEngine::SqlServer => "sqlserver",
        DbEngine::Odbc => "odbc",
    }
}

/// A value in an ODBC connection string: in braces when it has `;`, `{` or `}`.
fn odbc_value(v: &str) -> String {
    if v.contains([';', '{', '}']) || v.starts_with(' ') || v.ends_with(' ') {
        format!("{{{}}}", v.replace('}', "}}"))
    } else {
        v.to_string()
    }
}

/// A name of a table or a column, quoted in the dialect: letters, digits, `_` and `$` only.
fn quote(engine: DbEngine, name: &str) -> Result<String, String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !ok {
        return Err(format!(
            "`{name}` is not a name of a table or a column (letters, digits, `_`, `$`)"
        ));
    }
    Ok(match engine {
        DbEngine::Mysql => format!("`{name}`"),
        DbEngine::SqlServer => format!("[{name}]"),
        DbEngine::Postgres | DbEngine::Odbc => format!("\"{name}\""),
    })
}

/// A table, with its schema (`erp.orders`, `LIBRARY.ORDERS`), quoted.
fn quote_path(engine: DbEngine, path: &str) -> Result<String, String> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() > 2 {
        return Err(format!(
            "`{path}`: a table, or a schema and a table (`erp.orders`)"
        ));
    }
    Ok(parts
        .iter()
        .map(|p| quote(engine, p))
        .collect::<Result<Vec<_>, _>>()?
        .join("."))
}

/// What a pull reads: the table, its cursor column (quoted, and as named), a page's size.
struct Query {
    table: String,
    column: String,
    name: String,
    limit: usize,
}

/// Which rows, by the cursor column.
enum Where {
    /// the first pull: those with a value
    Start,
    /// `>=`
    From(String),
    /// `=`
    At(String),
    /// `>`
    After(String),
}

impl Where {
    fn value(&self) -> Option<&str> {
        match self {
            Where::Start => None,
            Where::From(v) | Where::At(v) | Where::After(v) => Some(v),
        }
    }

    /// The `SELECT` in the dialect; `param` is the placeholder of the value.
    fn sql(&self, engine: DbEngine, q: &Query, param: &str, limit: usize) -> String {
        let c = &q.column;
        let cond = match self {
            Where::Start => format!("{c} IS NOT NULL"),
            Where::From(_) => format!("{c} >= {param}"),
            Where::At(_) => format!("{c} = {param}"),
            Where::After(_) => format!("{c} > {param}"),
        };
        let t = &q.table;
        match engine {
            DbEngine::SqlServer => {
                format!("SELECT TOP ({limit}) * FROM {t} WHERE {cond} ORDER BY {c}")
            }
            DbEngine::Odbc => {
                format!("SELECT * FROM {t} WHERE {cond} ORDER BY {c} FETCH FIRST {limit} ROWS ONLY")
            }
            _ => format!("SELECT * FROM {t} WHERE {cond} ORDER BY {c} LIMIT {limit}"),
        }
    }
}

/// A row read: its columns, and the value of the cursor column as text (as it is bound back).
struct Row {
    values: Map<String, J>,
    cursor: Option<String>,
}

/// A column of a table: its name, and whether it holds an instant without a zone.
struct Column {
    name: String,
    instant: bool,
}

enum Insert {
    /// a unique key refused the row
    Duplicate(String),
    Failed(Failed),
}

impl From<Failed> for Insert {
    fn from(f: Failed) -> Self {
        Insert::Failed(f)
    }
}

// ---- values as JSON ------------------------------------------------------------------------

/// The largest integer a JSON number holds exactly everywhere (2^53 - 1).
const EXACT: i64 = 9_007_199_254_740_991;

/// An integer: a number when any reader holds it exactly, else its text.
fn int(i: i64) -> J {
    if (-EXACT..=EXACT).contains(&i) {
        J::from(i)
    } else {
        J::String(i.to_string())
    }
}

fn uint(u: u64) -> J {
    if u <= EXACT as u64 {
        J::from(u)
    } else {
        J::String(u.to_string())
    }
}

/// A floating point number (approximate in the database already); not a number: its text.
fn float(f: f64) -> J {
    serde_json::Number::from_f64(f)
        .map(J::Number)
        .unwrap_or_else(|| J::String(f.to_string()))
}

/// The fraction of a second: none, or its digits without trailing zeros.
fn frac(nanos: u32) -> String {
    if nanos == 0 {
        return String::new();
    }
    let s = format!(".{nanos:09}");
    s.trim_end_matches('0').to_string()
}

fn date_text(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn time_text(t: chrono::NaiveTime) -> String {
    use chrono::Timelike as _;
    format!("{}{}", t.format("%H:%M:%S"), frac(t.nanosecond()))
}

fn datetime_text(d: chrono::NaiveDateTime) -> String {
    format!("{}T{}", date_text(d.date()), time_text(d.time()))
}

/// A decimal of `value` × 10^-`scale`, exactly.
fn decimal_text(value: i128, scale: u8) -> String {
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let s = if scale == 0 {
        digits
    } else if digits.len() > scale {
        let (i, f) = digits.split_at(digits.len() - scale);
        format!("{i}.{f}")
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn base64(b: &[u8]) -> J {
    J::String(base64::engine::general_purpose::STANDARD.encode(b))
}

/// The text of a value as bound back for the cursor.
fn cursor_text(j: &J) -> Option<String> {
    match j {
        J::Null => None,
        J::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ---- connections -----------------------------------------------------------------------------

type Ms = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

enum Session {
    Pg(tokio_postgres::Client),
    My(mysql_async::Conn),
    Ms(Box<Ms>),
    #[cfg(feature = "odbc")]
    Odbc(Option<Box<odbc_api::Connection<'static>>>),
}

/// Connections open and idle, by [`Target::key`].
static IDLE: Mutex<Vec<([u8; 32], Instant, Session)>> = Mutex::new(Vec::new());

impl Session {
    /// A connection idle for this target that still answers, else a new one.
    async fn open(t: &Target, public_only: bool) -> Result<Session, Failed> {
        let key = t.key(public_only);
        loop {
            let found = {
                let mut idle = IDLE.lock().unwrap();
                idle.retain(|(_, at, _)| at.elapsed() < IDLE_FOR);
                idle.iter()
                    .rposition(|(k, _, _)| *k == key)
                    .map(|i| idle.remove(i).2)
            };
            match found {
                None => break,
                Some(mut s) => {
                    if s.alive(t).await {
                        return Ok(s);
                    }
                }
            }
        }
        Session::connect(t, public_only).await
    }

    /// Kept for the next pull or delivery, unless it failed in a way that may have broken it.
    fn keep_unless<T>(self, t: &Target, public_only: bool, r: &Result<T, Failed>) {
        if matches!(r, Err(Failed::Again(_))) {
            return;
        }
        let mut idle = IDLE.lock().unwrap();
        idle.push((t.key(public_only), Instant::now(), self));
        while idle.len() > IDLE_MAX {
            idle.remove(0);
        }
    }

    async fn alive(&mut self, t: &Target) -> bool {
        let probe = async {
            match self {
                Session::Pg(c) => !c.is_closed() && c.simple_query("").await.is_ok(),
                Session::My(c) => {
                    use mysql_async::prelude::Queryable as _;
                    c.ping().await.is_ok()
                }
                Session::Ms(c) => match c.simple_query("SELECT 1").await {
                    Ok(s) => s.into_results().await.is_ok(),
                    Err(_) => false,
                },
                #[cfg(feature = "odbc")]
                Session::Odbc(c) => c.as_ref().is_some_and(|c| !c.is_dead().unwrap_or(true)),
            }
        };
        tokio::time::timeout(t.timeout.min(Duration::from_secs(5)), probe)
            .await
            .unwrap_or(false)
    }

    async fn connect(t: &Target, public_only: bool) -> Result<Session, Failed> {
        match t.engine {
            DbEngine::Postgres => pg::connect(t, public_only).await.map(Session::Pg),
            DbEngine::Mysql => my::connect(t, public_only).await.map(Session::My),
            DbEngine::SqlServer => ms::connect(t, public_only)
                .await
                .map(|c| Session::Ms(Box::new(c))),
            DbEngine::Odbc => {
                #[cfg(feature = "odbc")]
                {
                    odbc::connect(t)
                        .await
                        .map(|c| Session::Odbc(Some(Box::new(c))))
                }
                #[cfg(not(feature = "odbc"))]
                {
                    Err(Failed::Waiting(format!(
                        "{}: this server has no ODBC: run the connection in an agent (location = \"agent:<name>\" in quadra.toml), where the driver is installed",
                        t.at()
                    )))
                }
            }
        }
    }

    async fn select(
        &mut self,
        t: &Target,
        q: &Query,
        w: &Where,
        limit: usize,
    ) -> Result<Vec<Row>, Failed> {
        match self {
            Session::Pg(c) => t.within(pg::select(c, t, q, w, limit)).await,
            Session::My(c) => t.within(my::select(c, t, q, w, limit)).await,
            Session::Ms(c) => t.within(ms::select(c, t, q, w, limit)).await,
            #[cfg(feature = "odbc")]
            Session::Odbc(c) => {
                let sql = w.sql(t.engine, q, "?", limit);
                let (value, column, secs) = (
                    w.value().map(|v| v.to_string()),
                    q.name.clone(),
                    t.timeout.as_secs() as usize,
                );
                let at = t.at();
                odbc::run(c, t, move |c| {
                    odbc::select(c, &at, &sql, value, &column, secs)
                })
                .await
            }
        }
    }

    async fn columns(&mut self, t: &Target, table: &str) -> Result<Vec<Column>, Failed> {
        match self {
            Session::Pg(c) => t.within(pg::columns(c, t, table)).await,
            Session::My(c) => t.within(my::columns(c, t, table)).await,
            Session::Ms(c) => t.within(ms::columns(c, t, table)).await,
            #[cfg(feature = "odbc")]
            Session::Odbc(c) => {
                let (sql, at) = (format!("SELECT * FROM {table} WHERE 1 = 0"), t.at());
                odbc::run(c, t, move |c| odbc::columns(c, &at, &sql)).await
            }
        }
    }

    /// Whether `table` has a row whose `column` is `value`.
    async fn has(
        &mut self,
        t: &Target,
        table: &str,
        column: &str,
        value: &str,
    ) -> Result<bool, Failed> {
        match self {
            Session::Pg(c) => {
                let sql = format!("SELECT 1 FROM {table} WHERE {column} = $1 LIMIT 1");
                t.within(async {
                    c.query_opt(&sql, &[&pg::Text(Some(value))])
                        .await
                        .map(|r| r.is_some())
                        .map_err(|e| pg::err(t, e))
                })
                .await
            }
            Session::My(c) => {
                use mysql_async::prelude::Queryable as _;
                let sql = format!("SELECT 1 FROM {table} WHERE {column} = ? LIMIT 1");
                t.within(async {
                    c.exec_first::<mysql_async::Row, _, _>(sql, (value,))
                        .await
                        .map(|r| r.is_some())
                        .map_err(|e| my::err(t, e))
                })
                .await
            }
            Session::Ms(c) => {
                let sql = format!("SELECT TOP (1) 1 FROM {table} WHERE {column} = @P1");
                t.within(async {
                    let s = c.query(sql, &[&value]).await.map_err(|e| ms::err(t, e))?;
                    s.into_first_result()
                        .await
                        .map(|r| !r.is_empty())
                        .map_err(|e| ms::err(t, e))
                })
                .await
            }
            #[cfg(feature = "odbc")]
            Session::Odbc(c) => {
                let (sql, at, secs, value) = (
                    format!("SELECT 1 FROM {table} WHERE {column} = ? FETCH FIRST 1 ROWS ONLY"),
                    t.at(),
                    t.timeout.as_secs() as usize,
                    value.to_string(),
                );
                odbc::run(c, t, move |c| odbc::has(c, &at, &sql, value, secs)).await
            }
        }
    }

    async fn insert(
        &mut self,
        t: &Target,
        table: &str,
        names: &[String],
        values: &[Option<String>],
    ) -> Result<(), Insert> {
        let marks: Vec<String> = (1..=values.len())
            .map(|i| match t.engine {
                DbEngine::Postgres => format!("${i}"),
                DbEngine::SqlServer => format!("@P{i}"),
                _ => "?".to_string(),
            })
            .collect();
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            marks.join(", ")
        );
        let r = match self {
            Session::Pg(c) => tokio::time::timeout(t.timeout, pg::insert(c, t, &sql, values)).await,
            Session::My(c) => tokio::time::timeout(t.timeout, my::insert(c, t, &sql, values)).await,
            Session::Ms(c) => tokio::time::timeout(t.timeout, ms::insert(c, t, &sql, values)).await,
            #[cfg(feature = "odbc")]
            Session::Odbc(c) => {
                let (at, secs, values) = (t.at(), t.timeout.as_secs() as usize, values.to_vec());
                return odbc::run(c, t, move |c| Ok(odbc::insert(c, &at, &sql, values, secs)))
                    .await?;
            }
        };
        r.unwrap_or_else(|_| {
            Err(Insert::Failed(t.again(format!(
                "no answer within {} s",
                t.timeout.as_secs()
            ))))
        })
    }
}

/// A TCP connection to the first address of the target that answers (only public ones with
/// `public_only`), each within the timeout.
async fn tcp(t: &Target, public_only: bool) -> Result<tokio::net::TcpStream, Failed> {
    let addrs = addrs(t, public_only).await?;
    let mut last = String::new();
    for a in addrs {
        match tokio::time::timeout(t.timeout, tokio::net::TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = format!("no answer within {} s", t.timeout.as_secs()),
        }
    }
    Err(t.again(format!("cannot connect: {last}")))
}

async fn addrs(t: &Target, public_only: bool) -> Result<Vec<std::net::SocketAddr>, Failed> {
    crate::deliver::addresses(&t.host, t.port, public_only)
        .await
        .map_err(|e| {
            if e.contains("not a public address") {
                t.never(format!(
                    "{e}; a database in a private network is reached through an agent (location = \"agent:<name>\")"
                ))
            } else {
                t.again(e)
            }
        })
}

/// The TLS of the connections: the system's trusted authorities.
fn tls_config() -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt as _;
    rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_platform_verifier()
    .map_err(|e| e.to_string())
    .map(|b| b.with_no_client_auth())
}

const TLS_HINT: &str =
    "(TLS: a database without it, on a private network through an agent, takes `insecure = true`)";

// ---- PostgreSQL ------------------------------------------------------------------------------

mod pg {
    use super::*;
    use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type};

    pub async fn connect(t: &Target, public_only: bool) -> Result<tokio_postgres::Client, Failed> {
        let stream = tcp(t, public_only).await?;
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&t.host)
            .port(t.port)
            .user(&t.user)
            .password(&t.password)
            .application_name("quadra")
            .connect_timeout(t.timeout);
        if !t.database.is_empty() {
            cfg.dbname(&t.database);
        }
        if t.insecure {
            cfg.ssl_mode(tokio_postgres::config::SslMode::Disable);
            let (c, conn) = t
                .within(async {
                    cfg.connect_raw(stream, tokio_postgres::NoTls)
                        .await
                        .map_err(|e| err(t, e))
                })
                .await?;
            tokio::spawn(conn);
            Ok(c)
        } else {
            use tokio_postgres::tls::MakeTlsConnect;
            cfg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let tls = tls_config().map_err(|e| t.never(format!("TLS: {e}")))?;
            let mut make = tokio_postgres_rustls::MakeRustlsConnect::new(tls);
            let tls = MakeTlsConnect::<tokio::net::TcpStream>::make_tls_connect(&mut make, &t.host)
                .map_err(|e| t.never(format!("TLS: {e}")))?;
            let (c, conn) = t
                .within(async { cfg.connect_raw(stream, tls).await.map_err(|e| err(t, e)) })
                .await?;
            tokio::spawn(conn);
            Ok(c)
        }
    }

    pub fn err(t: &Target, e: tokio_postgres::Error) -> Failed {
        if let Some(db) = e.as_db_error() {
            let code = db.code().code();
            let hint = match code {
                "42P01" | "42703" => " (the name is quoted, so as written: one created without quotes is in lower case)",
                "28P01" | "28000" => " (the user or the password)",
                _ => "",
            };
            let why = format!("{}{hint}", db.message());
            let again = code.starts_with("08")
                || code.starts_with("53")
                || code.starts_with("57")
                || matches!(code, "40001" | "40P01" | "55P03");
            return if again { t.again(why) } else { t.never(why) };
        }
        let text = e.to_string();
        let source = std::error::Error::source(&e)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let full = if source.is_empty() {
            text.clone()
        } else {
            format!("{text}: {source}")
        };
        if text.contains("TLS") || text.contains("tls") || source.contains("certificate") {
            t.never(format!("{full} {TLS_HINT}"))
        } else {
            t.again(full)
        }
    }

    /// A value bound as text, the database reading it in the type of its place (a cursor, a
    /// column): exact for decimals, dates and instants.
    #[derive(Debug)]
    pub struct Text<'a>(pub Option<&'a str>);

    impl ToSql for Text<'_> {
        fn to_sql(
            &self,
            _: &Type,
            out: &mut bytes::BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            match self.0 {
                None => Ok(IsNull::Yes),
                Some(s) => {
                    out.extend_from_slice(s.as_bytes());
                    Ok(IsNull::No)
                }
            }
        }

        fn accepts(_: &Type) -> bool {
            true
        }

        fn encode_format(&self, _: &Type) -> tokio_postgres::types::Format {
            tokio_postgres::types::Format::Text
        }

        tokio_postgres::types::to_sql_checked!();
    }

    /// A value read, as JSON, or why it cannot be.
    struct Value(Result<J, String>);

    impl<'a> FromSql<'a> for Value {
        fn from_sql(
            ty: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(Value(json(ty, raw)))
        }

        fn from_sql_null(_: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(Value(Ok(J::Null)))
        }

        fn accepts(_: &Type) -> bool {
            true
        }
    }

    fn be<const N: usize>(raw: &[u8]) -> Result<[u8; N], String> {
        raw.get(..N)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| "a value too short".to_string())
    }

    fn epoch() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    /// A value of PostgreSQL's binary format as JSON.
    fn json(ty: &Type, raw: &[u8]) -> Result<J, String> {
        let text = || String::from_utf8_lossy(raw).into_owned();
        Ok(match *ty {
            Type::BOOL => J::Bool(raw.first().is_some_and(|b| *b != 0)),
            Type::INT2 => J::from(i16::from_be_bytes(be(raw)?)),
            Type::INT4 => J::from(i32::from_be_bytes(be(raw)?)),
            Type::INT8 => int(i64::from_be_bytes(be(raw)?)),
            Type::OID => J::from(u32::from_be_bytes(be(raw)?)),
            Type::FLOAT4 => float(f32::from_be_bytes(be(raw)?) as f64),
            Type::FLOAT8 => float(f64::from_be_bytes(be(raw)?)),
            Type::NUMERIC => J::String(numeric(raw)?),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::XML | Type::CHAR => {
                J::String(text())
            }
            Type::JSON => serde_json::from_slice(raw).unwrap_or_else(|_| J::String(text())),
            Type::JSONB => {
                serde_json::from_slice(raw.get(1..).unwrap_or_default()).unwrap_or(J::Null)
            }
            Type::UUID => {
                let h = hex::encode(raw);
                if h.len() != 32 {
                    return Err("a uuid of 16 bytes".into());
                }
                J::String(format!(
                    "{}-{}-{}-{}-{}",
                    &h[..8],
                    &h[8..12],
                    &h[12..16],
                    &h[16..20],
                    &h[20..]
                ))
            }
            Type::BYTEA => base64(raw),
            Type::DATE => match i32::from_be_bytes(be(raw)?) {
                i32::MAX => J::String("infinity".into()),
                i32::MIN => J::String("-infinity".into()),
                d => J::String(date_text(epoch().date() + chrono::Duration::days(d as i64))),
            },
            Type::TIME => {
                let us = i64::from_be_bytes(be(raw)?);
                J::String(time_text(
                    chrono::NaiveTime::MIN + chrono::Duration::microseconds(us),
                ))
            }
            Type::TIMESTAMP | Type::TIMESTAMPTZ => match i64::from_be_bytes(be(raw)?) {
                i64::MAX => J::String("infinity".into()),
                i64::MIN => J::String("-infinity".into()),
                us => {
                    let d = datetime_text(epoch() + chrono::Duration::microseconds(us));
                    J::String(if *ty == Type::TIMESTAMPTZ {
                        format!("{d}Z")
                    } else {
                        d
                    })
                }
            },
            _ => match ty.kind() {
                Kind::Enum(_) => J::String(text()),
                Kind::Domain(inner) => return json(inner, raw),
                _ if matches!(ty.name(), "citext" | "ltree") => J::String(text()),
                _ => return Err(format!("of type {}", ty.name())),
            },
        })
    }

    /// A NUMERIC of the binary format, as its exact text.
    pub(super) fn numeric(raw: &[u8]) -> Result<String, String> {
        let at = |i: usize| -> Result<u16, String> {
            Ok(u16::from_be_bytes(be(raw.get(i..).unwrap_or_default())?))
        };
        let ndigits = at(0)? as usize;
        let weight = at(2)? as i16 as i64;
        let sign = at(4)?;
        let dscale = at(6)? as usize;
        match sign {
            0xC000 => return Ok("NaN".into()),
            0xD000 => return Ok("Infinity".into()),
            0xF000 => return Ok("-Infinity".into()),
            _ => {}
        }
        let mut digits = Vec::with_capacity(ndigits);
        for i in 0..ndigits {
            digits.push(at(8 + 2 * i)?);
        }
        // the base 10000 digit of weight `w`
        let group = |w: i64| -> u16 {
            let p = weight - w;
            if p >= 0 && (p as usize) < digits.len() {
                digits[p as usize]
            } else {
                0
            }
        };
        let mut s = String::new();
        if weight < 0 {
            s.push('0');
        } else {
            for w in (0..=weight).rev() {
                if w == weight {
                    s.push_str(&group(w).to_string());
                } else {
                    s.push_str(&format!("{:04}", group(w)));
                }
            }
        }
        if dscale > 0 {
            let mut f = String::new();
            let mut w = -1;
            while f.len() < dscale {
                f.push_str(&format!("{:04}", group(w)));
                w -= 1;
            }
            f.truncate(dscale);
            s.push('.');
            s.push_str(&f);
        }
        let zero = s.chars().all(|c| c == '0' || c == '.');
        if sign == 0x4000 && !zero {
            s.insert(0, '-');
        }
        Ok(s)
    }

    pub async fn select(
        c: &mut tokio_postgres::Client,
        t: &Target,
        q: &Query,
        w: &Where,
        limit: usize,
    ) -> Result<Vec<Row>, Failed> {
        let sql = w.sql(t.engine, q, "$1", limit);
        let stmt = c.prepare(&sql).await.map_err(|e| err(t, e))?;
        let value = w.value();
        let param = Text(value);
        let params: Vec<&(dyn ToSql + Sync)> = if value.is_some() {
            vec![&param]
        } else {
            vec![]
        };
        let rows = c.query(&stmt, &params).await.map_err(|e| err(t, e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let mut values = Map::new();
            let mut cursor = None;
            for (i, col) in r.columns().iter().enumerate() {
                let v = r
                    .try_get::<_, Value>(i)
                    .map_err(|e| t.never(e.to_string()))?
                    .0
                    .map_err(|ty| {
                        t.never(format!(
                            "the column {} is {ty}, which is not read: read it through a view that casts it (`CAST({} AS text)`)",
                            col.name(),
                            col.name()
                        ))
                    })?;
                if col.name() == q.name {
                    cursor = cursor_text(&v);
                }
                values.insert(col.name().to_string(), v);
            }
            out.push(Row { values, cursor });
        }
        Ok(out)
    }

    pub async fn columns(
        c: &mut tokio_postgres::Client,
        t: &Target,
        table: &str,
    ) -> Result<Vec<Column>, Failed> {
        let stmt = c
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
            .await
            .map_err(|e| err(t, e))?;
        Ok(stmt
            .columns()
            .iter()
            .map(|c| Column {
                name: c.name().to_string(),
                instant: false,
            })
            .collect())
    }

    pub async fn insert(
        c: &mut tokio_postgres::Client,
        t: &Target,
        sql: &str,
        values: &[Option<String>],
    ) -> Result<(), Insert> {
        let params: Vec<Text> = values.iter().map(|v| Text(v.as_deref())).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        match c.execute(sql, &refs).await {
            Ok(_) => Ok(()),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => {
                Err(Insert::Duplicate(
                    e.as_db_error()
                        .map(|d| d.message().to_string())
                        .unwrap_or_default(),
                ))
            }
            Err(e) => Err(Insert::Failed(err(t, e))),
        }
    }
}

// ---- MySQL -----------------------------------------------------------------------------------

mod my {
    use super::*;
    use mysql_async::consts::{ColumnFlags, ColumnType};
    use mysql_async::prelude::Queryable as _;
    use mysql_async::Value;

    pub async fn connect(t: &Target, public_only: bool) -> Result<mysql_async::Conn, Failed> {
        let mut last = None;
        for a in addrs(t, public_only).await? {
            let mut o = mysql_async::OptsBuilder::default()
                .ip_or_hostname(a.ip().to_string())
                .tcp_port(a.port())
                .user(Some(&t.user))
                .pass(Some(&t.password))
                .prefer_socket(false)
                .stmt_cache_size(64)
                // TIMESTAMP columns in UTC, as their cursor is bound back
                .init(vec!["SET time_zone = '+00:00'"]);
            if !t.database.is_empty() {
                o = o.db_name(Some(&t.database));
            }
            if !t.insecure {
                o = o.ssl_opts(
                    mysql_async::SslOpts::default()
                        .with_danger_tls_hostname_override(Some(t.host.clone())),
                );
            }
            match tokio::time::timeout(t.timeout, mysql_async::Conn::new(o)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) => match err(t, e) {
                    Failed::Again(why) => {
                        last = Some(Failed::Again(why.replacen(": ", ": cannot connect: ", 1)))
                    }
                    f => return Err(f),
                },
                Err(_) => {
                    last = Some(t.again(format!("no answer within {} s", t.timeout.as_secs())))
                }
            }
        }
        Err(last.unwrap_or_else(|| t.again("no address".into())))
    }

    pub fn err(t: &Target, e: mysql_async::Error) -> Failed {
        match e {
            mysql_async::Error::Server(s) => {
                let again = matches!(s.code, 1040 | 1203 | 1205 | 1213 | 1317 | 2006 | 2013);
                let hint = if s.code == 1045 {
                    " (the user or the password)"
                } else {
                    ""
                };
                let why = format!("{}{hint}", s.message);
                if again {
                    t.again(why)
                } else {
                    t.never(why)
                }
            }
            mysql_async::Error::Io(io) => {
                let text = io.to_string();
                if text.contains("certificate") || text.contains("TLS") || text.contains("tls") {
                    t.never(format!("{text} {TLS_HINT}"))
                } else {
                    t.again(text)
                }
            }
            mysql_async::Error::Driver(d) => {
                let text = d.to_string();
                if text.contains("SSL") || text.contains("TLS") {
                    t.never(format!("{text} {TLS_HINT}"))
                } else {
                    t.again(text)
                }
            }
            other => t.never(other.to_string()),
        }
    }

    /// How the cursor is bound for a column of this type: as text read in its type.
    fn cast(col: &mysql_async::Column) -> &'static str {
        let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
        match col.column_type() {
            ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_YEAR => {
                if unsigned {
                    "CAST(? AS UNSIGNED)"
                } else {
                    "CAST(? AS SIGNED)"
                }
            }
            ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                "CAST(? AS DECIMAL(65,30))"
            }
            ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => "CAST(? AS DOUBLE)",
            ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "CAST(? AS DATE)",
            ColumnType::MYSQL_TYPE_DATETIME
            | ColumnType::MYSQL_TYPE_DATETIME2
            | ColumnType::MYSQL_TYPE_TIMESTAMP
            | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "CAST(? AS DATETIME(6))",
            ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "CAST(? AS TIME(6))",
            _ => "?",
        }
    }

    fn is_timestamp(col: &mysql_async::Column) -> bool {
        matches!(
            col.column_type(),
            ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2
        )
    }

    /// A value of the binary protocol as JSON.
    fn json(v: &Value, col: &mysql_async::Column) -> J {
        match v {
            Value::NULL => J::Null,
            Value::Int(i) => int(*i),
            Value::UInt(u) => uint(*u),
            Value::Float(f) => float(*f as f64),
            Value::Double(d) => float(*d),
            Value::Date(y, mo, d, h, mi, s, us) => {
                if matches!(
                    col.column_type(),
                    ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE
                ) {
                    J::String(format!("{y:04}-{mo:02}-{d:02}"))
                } else {
                    let z = if is_timestamp(col) { "Z" } else { "" };
                    J::String(format!(
                        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{}{z}",
                        frac(us * 1000)
                    ))
                }
            }
            Value::Time(neg, days, h, mi, s, us) => J::String(format!(
                "{}{:02}:{mi:02}:{s:02}{}",
                if *neg { "-" } else { "" },
                *days as u64 * 24 + *h as u64,
                frac(us * 1000)
            )),
            Value::Bytes(b) => match col.column_type() {
                ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                    J::String(String::from_utf8_lossy(b).into_owned())
                }
                ColumnType::MYSQL_TYPE_JSON => serde_json::from_slice(b)
                    .unwrap_or_else(|_| J::String(String::from_utf8_lossy(b).into_owned())),
                ColumnType::MYSQL_TYPE_BIT if b.len() <= 8 => {
                    uint(b.iter().fold(0u64, |a, x| (a << 8) | *x as u64))
                }
                _ if col.character_set() == 63 => base64(b),
                _ => J::String(String::from_utf8_lossy(b).into_owned()),
            },
        }
    }

    pub async fn select(
        c: &mut mysql_async::Conn,
        t: &Target,
        q: &Query,
        w: &Where,
        limit: usize,
    ) -> Result<Vec<Row>, Failed> {
        // the type of the cursor column, for its value to be compared in it
        let probe = c
            .prep(format!("SELECT {} FROM {} LIMIT 0", q.column, q.table))
            .await
            .map_err(|e| err(t, e))?;
        let param = probe.columns().first().map(cast).unwrap_or("?");
        let sql = w.sql(t.engine, q, param, limit);
        let params = match w.value() {
            Some(v) => mysql_async::Params::Positional(vec![Value::Bytes(v.as_bytes().to_vec())]),
            None => mysql_async::Params::Empty,
        };
        let rows: Vec<mysql_async::Row> = c.exec(sql, params).await.map_err(|e| err(t, e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let cols = r.columns_ref();
            let mut values = Map::new();
            let mut cursor = None;
            for (i, col) in cols.iter().enumerate() {
                let v = r.as_ref(i).map(|v| json(v, col)).unwrap_or(J::Null);
                let name = col.name_str().into_owned();
                if name == q.name {
                    cursor = cursor_text(&v).map(|s| {
                        if is_timestamp(col) {
                            s.trim_end_matches('Z').to_string()
                        } else {
                            s
                        }
                    });
                }
                values.insert(name, v);
            }
            out.push(Row { values, cursor });
        }
        Ok(out)
    }

    pub async fn columns(
        c: &mut mysql_async::Conn,
        t: &Target,
        table: &str,
    ) -> Result<Vec<Column>, Failed> {
        let stmt = c
            .prep(format!("SELECT * FROM {table} LIMIT 0"))
            .await
            .map_err(|e| err(t, e))?;
        Ok(stmt
            .columns()
            .iter()
            .map(|c| Column {
                name: c.name_str().into_owned(),
                instant: matches!(
                    c.column_type(),
                    ColumnType::MYSQL_TYPE_DATETIME
                        | ColumnType::MYSQL_TYPE_DATETIME2
                        | ColumnType::MYSQL_TYPE_TIMESTAMP
                        | ColumnType::MYSQL_TYPE_TIMESTAMP2
                ),
            })
            .collect())
    }

    pub async fn insert(
        c: &mut mysql_async::Conn,
        t: &Target,
        sql: &str,
        values: &[Option<String>],
    ) -> Result<(), Insert> {
        let params: Vec<Value> = values
            .iter()
            .map(|v| match v {
                Some(s) => Value::Bytes(s.as_bytes().to_vec()),
                None => Value::NULL,
            })
            .collect();
        match c
            .exec_drop(sql, mysql_async::Params::Positional(params))
            .await
        {
            Ok(()) => Ok(()),
            Err(mysql_async::Error::Server(s)) if matches!(s.code, 1062 | 1586) => {
                Err(Insert::Duplicate(s.message))
            }
            Err(e) => Err(Insert::Failed(err(t, e))),
        }
    }
}

// ---- SQL Server ------------------------------------------------------------------------------

mod ms {
    use super::*;
    use tiberius::{ColumnData, ColumnType, FromSql as _};
    use tokio_util::compat::TokioAsyncWriteCompatExt as _;

    pub async fn connect(t: &Target, public_only: bool) -> Result<Ms, Failed> {
        let stream = tcp(t, public_only).await?;
        let mut cfg = tiberius::Config::new();
        cfg.host(&t.host);
        cfg.port(t.port);
        if !t.database.is_empty() {
            cfg.database(&t.database);
        }
        cfg.application_name("quadra");
        cfg.authentication(tiberius::AuthMethod::sql_server(&t.user, &t.password));
        cfg.encryption(if t.insecure {
            tiberius::EncryptionLevel::NotSupported
        } else {
            tiberius::EncryptionLevel::Required
        });
        t.within(async {
            tiberius::Client::connect(cfg, stream.compat_write())
                .await
                .map_err(|e| err(t, e))
        })
        .await
    }

    pub fn err(t: &Target, e: tiberius::error::Error) -> Failed {
        match &e {
            tiberius::error::Error::Server(s) => {
                let hint = if s.code() == 18456 {
                    " (the user or the password)"
                } else {
                    ""
                };
                let why = format!("{}{hint}", s.message());
                if matches!(
                    s.code(),
                    1205 | 1222 | 40501 | 40613 | 49918 | 49919 | 49920
                ) {
                    t.again(why)
                } else {
                    t.never(why)
                }
            }
            tiberius::error::Error::Io { .. } => t.again(e.to_string()),
            // its first sentence: the rest is advice about the driver's own settings
            tiberius::error::Error::Tls(_) => {
                let text = e.to_string();
                let first = text.split(". ").next().unwrap_or(&text);
                t.never(format!("{first} {TLS_HINT}"))
            }
            _ => t.never(e.to_string()),
        }
    }

    fn base_1900() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(1900, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    /// A value of SQL Server as JSON.
    fn json(v: &ColumnData<'static>) -> J {
        match v {
            ColumnData::U8(Some(x)) => J::from(*x),
            ColumnData::I16(Some(x)) => J::from(*x),
            ColumnData::I32(Some(x)) => J::from(*x),
            ColumnData::I64(Some(x)) => int(*x),
            ColumnData::F32(Some(x)) => float(*x as f64),
            ColumnData::F64(Some(x)) => float(*x),
            ColumnData::Bit(Some(b)) => J::Bool(*b),
            ColumnData::String(Some(s)) => J::String(s.to_string()),
            ColumnData::Guid(Some(g)) => J::String(g.to_string().to_uppercase()),
            ColumnData::Binary(Some(b)) => base64(b),
            ColumnData::Numeric(Some(n)) => J::String(decimal_text(n.value(), n.scale())),
            ColumnData::Xml(Some(x)) => J::String(x.to_string()),
            // 1/300 of a second: as milliseconds, which the type reads back the same
            ColumnData::DateTime(Some(d)) => {
                let ms = (d.seconds_fragments() as i64 * 10 + 1) / 3;
                J::String(datetime_text(
                    base_1900()
                        + chrono::Duration::days(d.days() as i64)
                        + chrono::Duration::milliseconds(ms),
                ))
            }
            ColumnData::SmallDateTime(Some(_)) | ColumnData::DateTime2(Some(_)) => {
                match chrono::NaiveDateTime::from_sql(v) {
                    Ok(Some(d)) => J::String(datetime_text(d)),
                    _ => J::Null,
                }
            }
            ColumnData::Date(Some(_)) => match chrono::NaiveDate::from_sql(v) {
                Ok(Some(d)) => J::String(date_text(d)),
                _ => J::Null,
            },
            ColumnData::Time(Some(_)) => match chrono::NaiveTime::from_sql(v) {
                Ok(Some(d)) => J::String(time_text(d)),
                _ => J::Null,
            },
            ColumnData::DateTimeOffset(Some(_)) => {
                match chrono::DateTime::<chrono::FixedOffset>::from_sql(v) {
                    Ok(Some(d)) => J::String(format!(
                        "{}{}",
                        datetime_text(d.naive_local()),
                        d.format("%:z")
                    )),
                    _ => J::Null,
                }
            }
            _ => J::Null,
        }
    }

    pub async fn select(
        c: &mut Ms,
        t: &Target,
        q: &Query,
        w: &Where,
        limit: usize,
    ) -> Result<Vec<Row>, Failed> {
        let sql = w.sql(t.engine, q, "@P1", limit);
        let value = w.value();
        let params: Vec<&dyn tiberius::ToSql> = match &value {
            Some(v) => vec![v],
            None => vec![],
        };
        let rows = c
            .query(sql, &params)
            .await
            .map_err(|e| err(t, e))?
            .into_first_result()
            .await
            .map_err(|e| err(t, e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let names: Vec<String> = r.columns().iter().map(|c| c.name().to_string()).collect();
            let mut values = Map::new();
            let mut cursor = None;
            for (name, v) in names.into_iter().zip(r) {
                let j = json(&v);
                if name == q.name {
                    cursor = cursor_text(&j);
                }
                values.insert(name, j);
            }
            out.push(Row { values, cursor });
        }
        Ok(out)
    }

    pub async fn columns(c: &mut Ms, t: &Target, table: &str) -> Result<Vec<Column>, Failed> {
        let mut s = c
            .simple_query(format!("SELECT TOP (0) * FROM {table}"))
            .await
            .map_err(|e| err(t, e))?;
        let cols = s
            .columns()
            .await
            .map_err(|e| err(t, e))?
            .unwrap_or_default()
            .iter()
            .map(|c| Column {
                name: c.name().to_string(),
                instant: matches!(
                    c.column_type(),
                    ColumnType::Datetime
                        | ColumnType::Datetimen
                        | ColumnType::Datetime4
                        | ColumnType::Datetime2
                ),
            })
            .collect();
        s.into_results().await.map_err(|e| err(t, e))?;
        Ok(cols)
    }

    pub async fn insert(
        c: &mut Ms,
        t: &Target,
        sql: &str,
        values: &[Option<String>],
    ) -> Result<(), Insert> {
        let params: Vec<Option<&str>> = values.iter().map(|v| v.as_deref()).collect();
        let refs: Vec<&dyn tiberius::ToSql> =
            params.iter().map(|p| p as &dyn tiberius::ToSql).collect();
        match c.execute(sql, &refs).await {
            Ok(_) => Ok(()),
            Err(tiberius::error::Error::Server(s)) if matches!(s.code(), 2601 | 2627) => {
                Err(Insert::Duplicate(s.message().to_string()))
            }
            Err(e) => Err(Insert::Failed(err(t, e))),
        }
    }
}

// ---- ODBC ------------------------------------------------------------------------------------

#[cfg(feature = "odbc")]
mod odbc {
    use super::*;
    use odbc_api::buffers::TextRowSet;
    use odbc_api::parameter::VarCharBox;
    use odbc_api::{DataType, IntoParameter as _, ResultSetMetadata as _};
    use std::sync::OnceLock;

    type Conn = odbc_api::Connection<'static>;

    /// The driver manager (unixODBC), once per process.
    fn environment() -> Result<&'static odbc_api::Environment, String> {
        static ENV: OnceLock<Result<odbc_api::Environment, String>> = OnceLock::new();
        ENV.get_or_init(|| odbc_api::Environment::new().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| format!("no ODBC driver manager (unixODBC): {e}"))
    }

    pub async fn connect(t: &Target) -> Result<Conn, Failed> {
        let (cs, at, secs) = (t.odbc.clone(), t.at(), t.timeout.as_secs() as u32);
        tokio::task::spawn_blocking(move || {
            let env = environment().map_err(|e| Failed::Waiting(format!("{at}: {e}")))?;
            env.connect_with_connection_string(
                &cs,
                odbc_api::ConnectionOptions {
                    login_timeout_sec: Some(secs),
                    ..Default::default()
                },
            )
            .map_err(|e| err(&at, e))
        })
        .await
        .map_err(|e| t.again(e.to_string()))?
    }

    /// `f` with the connection, on a thread that may wait (ODBC blocks).
    pub async fn run<T: Send + 'static>(
        slot: &mut Option<Box<Conn>>,
        t: &Target,
        f: impl FnOnce(&Conn) -> Result<T, Failed> + Send + 'static,
    ) -> Result<T, Failed> {
        let c = slot
            .take()
            .ok_or_else(|| t.again("the connection was lost".into()))?;
        let (c, r) = tokio::task::spawn_blocking(move || {
            let r = f(&c);
            (c, r)
        })
        .await
        .map_err(|e| t.again(e.to_string()))?;
        *slot = Some(c);
        r
    }

    /// The SQLSTATE of an error, if the driver gave one.
    fn state(e: &odbc_api::Error) -> String {
        match e {
            odbc_api::Error::Diagnostics { record, .. } => {
                String::from_utf8_lossy(&record.state.0).into_owned()
            }
            _ => String::new(),
        }
    }

    pub fn err(at: &str, e: odbc_api::Error) -> Failed {
        let s = state(&e);
        let why = format!("{at}: {e}");
        if s.starts_with("08") || s.starts_with("HYT") || s == "40001" || s.starts_with("57") {
            Failed::Again(why)
        } else {
            Failed::Never(why)
        }
    }

    /// A value the driver wrote as text, as JSON by the column's type.
    fn json(ty: &DataType, text: &str) -> J {
        let t = text.trim();
        match ty {
            DataType::TinyInt | DataType::SmallInt | DataType::Integer | DataType::BigInt => t
                .parse::<i64>()
                .map(int)
                .unwrap_or_else(|_| J::String(t.to_string())),
            DataType::Real | DataType::Float { .. } | DataType::Double => t
                .parse::<f64>()
                .map(float)
                .unwrap_or_else(|_| J::String(t.to_string())),
            DataType::Decimal { .. } | DataType::Numeric { .. } => {
                J::String(if let Some(r) = t.strip_prefix("-.") {
                    format!("-0.{r}")
                } else if let Some(r) = t.strip_prefix('.') {
                    format!("0.{r}")
                } else {
                    t.to_string()
                })
            }
            DataType::Bit => J::Bool(t == "1"),
            DataType::Timestamp { .. } => J::String(t.replacen(' ', "T", 1)),
            DataType::Binary { .. }
            | DataType::Varbinary { .. }
            | DataType::LongVarbinary { .. } => hex::decode(t)
                .map(|b| base64(&b))
                .unwrap_or_else(|_| J::String(text.to_string())),
            _ => J::String(text.to_string()),
        }
    }

    fn rows(
        at: &str,
        mut cursor: impl odbc_api::Cursor,
        column: Option<&str>,
    ) -> Result<Vec<Row>, Failed> {
        let n = cursor.num_result_cols().map_err(|e| err(at, e))? as u16;
        let mut names = Vec::new();
        let mut types = Vec::new();
        for i in 1..=n {
            names.push(cursor.col_name(i).map_err(|e| err(at, e))?);
            types.push(cursor.col_data_type(i).map_err(|e| err(at, e))?);
        }
        let buffer =
            TextRowSet::for_cursor(500, &mut cursor, Some(65_536)).map_err(|e| err(at, e))?;
        let mut block = cursor.bind_buffer(buffer).map_err(|e| err(at, e))?;
        let mut out = Vec::new();
        while let Some(batch) = block.fetch().map_err(|e| err(at, e))? {
            for r in 0..batch.num_rows() {
                let mut values = Map::new();
                let mut cur = None;
                for (i, name) in names.iter().enumerate() {
                    let v = match batch.at(i, r) {
                        None => J::Null,
                        Some(b) => {
                            let text = String::from_utf8_lossy(b);
                            if Some(name.as_str()) == column {
                                cur = Some(text.trim_end().to_string());
                            }
                            json(&types[i], &text)
                        }
                    };
                    values.insert(name.clone(), v);
                }
                out.push(Row {
                    values,
                    cursor: cur,
                });
            }
        }
        Ok(out)
    }

    pub fn select(
        c: &Conn,
        at: &str,
        sql: &str,
        value: Option<String>,
        column: &str,
        secs: usize,
    ) -> Result<Vec<Row>, Failed> {
        let cursor = match &value {
            Some(v) => c.execute(sql, &v.as_str().into_parameter(), Some(secs)),
            None => c.execute(sql, (), Some(secs)),
        }
        .map_err(|e| err(at, e))?;
        match cursor {
            Some(cursor) => rows(at, cursor, Some(column)),
            None => Ok(Vec::new()),
        }
    }

    pub fn has(c: &Conn, at: &str, sql: &str, value: String, secs: usize) -> Result<bool, Failed> {
        let cursor = c
            .execute(sql, &value.as_str().into_parameter(), Some(secs))
            .map_err(|e| err(at, e))?;
        match cursor {
            Some(cursor) => Ok(!rows(at, cursor, None)?.is_empty()),
            None => Ok(false),
        }
    }

    pub fn columns(c: &Conn, at: &str, sql: &str) -> Result<Vec<Column>, Failed> {
        let mut p = c.prepare(sql).map_err(|e| err(at, e))?;
        let n = p.num_result_cols().map_err(|e| err(at, e))? as u16;
        let mut out = Vec::new();
        for i in 1..=n {
            out.push(Column {
                name: p.col_name(i).map_err(|e| err(at, e))?,
                instant: matches!(
                    p.col_data_type(i).map_err(|e| err(at, e))?,
                    DataType::Timestamp { .. }
                ),
            });
        }
        Ok(out)
    }

    pub fn insert(
        c: &Conn,
        at: &str,
        sql: &str,
        values: Vec<Option<String>>,
        secs: usize,
    ) -> Result<(), Insert> {
        let params: Vec<VarCharBox> = values
            .into_iter()
            .map(|v| match v {
                Some(s) => VarCharBox::from_string(s),
                None => VarCharBox::null(),
            })
            .collect();
        match c.execute(sql, &params[..], Some(secs)) {
            Ok(_) => Ok(()),
            Err(e) if state(&e).starts_with("23") => Err(Insert::Duplicate(e.to_string())),
            Err(e) => Err(Insert::Failed(err(at, e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_and_quoted_in_each_dialect() {
        assert_eq!(quote(DbEngine::Postgres, "ORDERS").unwrap(), "\"ORDERS\"");
        assert_eq!(quote(DbEngine::Mysql, "orders").unwrap(), "`orders`");
        assert_eq!(quote(DbEngine::SqlServer, "Orders").unwrap(), "[Orders]");
        assert_eq!(
            quote_path(DbEngine::Odbc, "ERPLIB.ORD$IN").unwrap(),
            "\"ERPLIB\".\"ORD$IN\""
        );
        for bad in [
            "",
            "a b",
            "a;drop table x",
            "a\"b",
            "a`b",
            "a]b",
            "1abc",
            "a--",
            "a.b.c",
            "a..b",
        ] {
            assert!(quote_path(DbEngine::Postgres, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_dialect_limits_the_page_its_way() {
        let q = |e| Query {
            table: quote_path(e, "erp.orders").unwrap(),
            column: quote(e, "updated_at").unwrap(),
            name: "updated_at".into(),
            limit: 1000,
        };
        assert_eq!(
            Where::From("x".into()).sql(DbEngine::Postgres, &q(DbEngine::Postgres), "$1", 1000),
            "SELECT * FROM \"erp\".\"orders\" WHERE \"updated_at\" >= $1 ORDER BY \"updated_at\" LIMIT 1000"
        );
        assert_eq!(
            Where::Start.sql(DbEngine::SqlServer, &q(DbEngine::SqlServer), "@P1", 10),
            "SELECT TOP (10) * FROM [erp].[orders] WHERE [updated_at] IS NOT NULL ORDER BY [updated_at]"
        );
        assert_eq!(
            Where::After("x".into()).sql(DbEngine::Odbc, &q(DbEngine::Odbc), "?", 5),
            "SELECT * FROM \"erp\".\"orders\" WHERE \"updated_at\" > ? ORDER BY \"updated_at\" FETCH FIRST 5 ROWS ONLY"
        );
        assert_eq!(
            Where::At("x".into()).sql(
                DbEngine::Mysql,
                &q(DbEngine::Mysql),
                "CAST(? AS SIGNED)",
                7
            ),
            "SELECT * FROM `erp`.`orders` WHERE `updated_at` = CAST(? AS SIGNED) ORDER BY `updated_at` LIMIT 7"
        );
    }

    #[test]
    fn values_are_exact_json() {
        assert_eq!(int(42), J::from(42));
        assert_eq!(int(9_007_199_254_740_993), J::from("9007199254740993"));
        assert_eq!(uint(u64::MAX), J::from("18446744073709551615"));
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(123456, 2), "1234.56");
        assert_eq!(decimal_text(7, 0), "7");
        assert_eq!(frac(120_000_000), ".12");
        assert_eq!(frac(0), "");
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 27)
            .unwrap()
            .and_hms_micro_opt(10, 14, 3, 500)
            .unwrap();
        assert_eq!(datetime_text(d), "2026-11-27T10:14:03.0005");
        // an instant into a column without a zone: UTC, as the database reads it
        let v = J::from("2026-11-27T11:14:03+01:00");
        assert_eq!(
            text_of(DbEngine::Mysql, &v, true).as_deref(),
            Some("2026-11-27T10:14:03")
        );
        assert_eq!(
            text_of(DbEngine::Odbc, &v, true).as_deref(),
            Some("2026-11-27 10:14:03")
        );
        assert_eq!(
            text_of(DbEngine::Postgres, &v, true).as_deref(),
            Some("2026-11-27T11:14:03+01:00")
        );
        assert_eq!(
            text_of(DbEngine::SqlServer, &J::Bool(true), false).as_deref(),
            Some("1")
        );
        assert_eq!(text_of(DbEngine::Postgres, &J::Null, false), None);
    }

    #[test]
    fn numerics_of_the_binary_format_are_read_exactly() {
        // 12345.6789 (dscale 4): weight 1, digits 1 2345 6789
        let raw = [0, 3, 0, 1, 0, 0, 0, 4, 0, 1, 0x09, 0x29, 0x1A, 0x85];
        assert_eq!(pg::numeric(&raw).unwrap(), "12345.6789");
        // -0.0012 (dscale 4): weight -1, digit 12
        let raw = [0, 1, 0xFF, 0xFF, 0x40, 0, 0, 4, 0, 12];
        assert_eq!(pg::numeric(&raw).unwrap(), "-0.0012");
        // 10.50 (dscale 2): weight 0, digits 10 5000
        let raw = [0, 2, 0, 0, 0, 0, 0, 2, 0, 10, 0x13, 0x88];
        assert_eq!(pg::numeric(&raw).unwrap(), "10.50");
        assert_eq!(pg::numeric(&[0, 0, 0, 0, 0, 0, 0, 0]).unwrap(), "0");
    }

    fn resolved(toml: &str) -> Resolved {
        let m = quadra_sema::manifest::parse(toml).unwrap();
        let connection = m.connection("erp").unwrap().clone();
        let mut env = std::collections::BTreeMap::new();
        env.insert("ERP_PASS".to_string(), "s3cret;x}".to_string());
        env.insert(
            "ERP_URL".to_string(),
            "postgres://me:pw@db:5432/erp".to_string(),
        );
        Resolved {
            manifest: m,
            connection,
            env,
        }
    }

    #[test]
    fn the_url_names_where_and_never_the_credentials() {
        let c = resolved("[vars]\nERP_PASS = { secret = true }\n\n[connections.erp]\nkind = \"postgres\"\nurl = \"postgres://db.example.com/erp\"\nuser = \"quadra\"\npassword = { var = \"ERP_PASS\" }\n");
        let t = Target::of(&c).unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.database.as_str()),
            ("db.example.com", 5432, "erp")
        );
        assert_eq!(t.at(), "`erp` (postgres db.example.com:5432)");
        let c = resolved("[vars]\nERP_PASS = { secret = true }\n\n[connections.erp]\nkind = \"odbc\"\nurl = \"DSN=AS400\"\nuser = \"QUADRA\"\npassword = { var = \"ERP_PASS\" }\n");
        let t = Target::of(&c).unwrap();
        assert_eq!(t.odbc, "DSN=AS400;UID=QUADRA;PWD={s3cret;x}}}");
        assert_eq!(t.at(), "`erp` (odbc)");
        // credentials in a url from a variable are refused, and not repeated
        let c = resolved("[vars]\nERP_URL = {}\n\n[connections.erp]\nkind = \"postgres\"\nurl = { var = \"ERP_URL\" }\n");
        let Err(Failed::Never(e)) = Target::of(&c) else {
            panic!()
        };
        assert!(e.contains("no credentials") && !e.contains("pw"), "{e}");
    }

    #[cfg(not(feature = "odbc"))]
    #[tokio::test]
    async fn without_odbc_the_server_sends_it_to_an_agent() {
        let c = resolved("[connections.erp]\nkind = \"odbc\"\nurl = \"DSN=AS400\"\n");
        let ask = Ask::Rows {
            table: "ORDERS".into(),
            column: Some("ID".into()),
            after: None,
            limit: 10,
        };
        let Err(Failed::Waiting(e)) = fetch(&c, &ask, false).await else {
            panic!()
        };
        assert!(e.contains("agent"), "{e}");
    }

    #[tokio::test]
    async fn a_private_address_is_refused_where_only_public_ones_are_reached() {
        let c = resolved("[connections.erp]\nkind = \"postgres\"\nurl = \"postgres://127.0.0.1:5432/erp\"\ninsecure = true\n");
        let ask = Ask::Rows {
            table: "orders".into(),
            column: Some("id".into()),
            after: None,
            limit: 10,
        };
        let Err(Failed::Never(e)) = fetch(&c, &ask, true).await else {
            panic!()
        };
        assert!(
            e.contains("not a public address") && e.contains("agent"),
            "{e}"
        );
    }
}
