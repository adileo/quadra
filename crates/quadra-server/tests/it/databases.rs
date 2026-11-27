//! The databases of other systems: a table read in pieces with its cursor, rows written to an
//! exchange table once per event, values exact. Each test runs against a real database when
//! its variable holds a url with credentials, and is skipped otherwise:
//!
//! - `QUADRA_TEST_PG`: `postgres://postgres:pass@127.0.0.1:5432/postgres`
//! - `QUADRA_TEST_MYSQL`: `mysql://root:pass@127.0.0.1:3306/erp`
//! - `QUADRA_TEST_MSSQL`: `sqlserver://sa:pass@127.0.0.1:1433/master`
//!
//! `QUADRA_TEST_DB_ROWS` sets the rows of the large table: 5,000 when not set, so that a debug
//! build runs it in minutes; the pass criterion is a million (`QUADRA_TEST_DB_ROWS=1000000
//! cargo test --release -p quadra-server --test it large_table -- --nocapture`), which the
//! inbox handles at a few hundred rows a second.

use quadra_sema::Project;
use quadra_server::transport::{run, Ask, Conn as Job0, Failed, Item, Job, Outcome, Put};
use quadra_server::{Config, Host, HostOptions, Limits, Outside, TenantResolver, TenantSpec};
use quadra_store::{auth, ops, Conn};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A database of the tests: its kind, its url without credentials, its user and password.
struct Db {
    kind: &'static str,
    url: String,
    user: String,
    password: String,
    /// the url with its credentials, for the tests' own setup
    full: String,
}

fn db(var: &str, kind: &'static str) -> Option<Db> {
    let Ok(full) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let u = reqwest::Url::parse(&full).expect("a url");
    let mut bare = u.clone();
    bare.set_username("").unwrap();
    bare.set_password(None).unwrap();
    Some(Db {
        kind,
        url: bare.to_string(),
        user: u.username().to_string(),
        password: u.password().unwrap_or_default().to_string(),
        full,
    })
}

fn pg() -> Option<Db> {
    db("QUADRA_TEST_PG", "postgres")
}

fn mysql() -> Option<Db> {
    db("QUADRA_TEST_MYSQL", "mysql")
}

fn mssql() -> Option<Db> {
    db("QUADRA_TEST_MSSQL", "sqlserver")
}

/// ODBC, on the PostgreSQL of `QUADRA_TEST_PG` (its tables made there): `QUADRA_TEST_ODBC` is
/// the connection string without credentials (`Driver={PostgreSQL Unicode};Server=…;Port=…;
/// Database=…`). Only where the `odbc` feature is built.
#[cfg_attr(not(feature = "odbc"), allow(dead_code))]
fn odbc() -> Option<Db> {
    let pg = db("QUADRA_TEST_PG", "postgres")?;
    let Ok(url) = std::env::var("QUADRA_TEST_ODBC") else {
        eprintln!("QUADRA_TEST_ODBC not set: skipped");
        return None;
    };
    Some(Db {
        kind: "odbc",
        url,
        user: pg.user,
        password: pg.password,
        full: pg.full,
    })
}

/// The SQL the test's own statements are written in: ODBC's database is a PostgreSQL.
fn dialect(d: &Db) -> &'static str {
    if d.kind == "odbc" {
        "postgres"
    } else {
        d.kind
    }
}

/// A name of this run, so that runs side by side do not meet.
fn unique(name: &str) -> String {
    format!("q{}_{name}", std::process::id())
}

/// Statements run on the database, for the setup of a test.
fn exec(d: &Db, stmts: &[String]) {
    match dialect(d) {
        "postgres" => {
            let mut c = postgres::Client::connect(&d.full, postgres::NoTls).unwrap();
            for s in stmts {
                c.batch_execute(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            }
        }
        "mysql" => block_on(async {
            use mysql_async::prelude::Queryable as _;
            let mut c = mysql_async::Conn::from_url(&d.full).await.unwrap();
            for s in stmts {
                c.query_drop(s).await.unwrap_or_else(|e| panic!("{s}: {e}"));
            }
            c.disconnect().await.unwrap();
        }),
        _ => block_on(async {
            let mut c = mssql_client(d).await;
            for s in stmts {
                c.simple_query(s.as_str())
                    .await
                    .unwrap_or_else(|e| panic!("{s}: {e}"))
                    .into_results()
                    .await
                    .unwrap();
            }
        }),
    }
}

/// `expr` as text, in the dialect.
fn text(d: &Db, expr: &str) -> String {
    match dialect(d) {
        "postgres" => format!("CAST({expr} AS TEXT)"),
        "mysql" => format!("CAST({expr} AS CHAR)"),
        _ => format!("CAST({expr} AS NVARCHAR(4000))"),
    }
}

/// The first column of the rows of a query, a text (see [`text`]).
fn query(d: &Db, sql: &str) -> Vec<String> {
    match dialect(d) {
        "postgres" => {
            let mut c = postgres::Client::connect(&d.full, postgres::NoTls).unwrap();
            c.query(sql, &[])
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
                .iter()
                .map(|r| r.get::<_, Option<String>>(0).unwrap_or_default())
                .collect()
        }
        "mysql" => block_on(async {
            use mysql_async::prelude::Queryable as _;
            let mut c = mysql_async::Conn::from_url(&d.full).await.unwrap();
            let r: Vec<Option<String>> = c.query(sql).await.unwrap();
            c.disconnect().await.unwrap();
            r.into_iter().map(|v| v.unwrap_or_default()).collect()
        }),
        _ => block_on(async {
            let mut c = mssql_client(d).await;
            c.simple_query(sql)
                .await
                .unwrap()
                .into_first_result()
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<&str, _>(0).unwrap_or_default().to_string())
                .collect()
        }),
    }
}

async fn mssql_client(
    d: &Db,
) -> tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>> {
    use tokio_util::compat::TokioAsyncWriteCompatExt as _;
    let u = reqwest::Url::parse(&d.full).unwrap();
    let mut cfg = tiberius::Config::new();
    cfg.host(u.host_str().unwrap());
    cfg.port(u.port().unwrap_or(1433));
    cfg.authentication(tiberius::AuthMethod::sql_server(&d.user, &d.password));
    cfg.encryption(tiberius::EncryptionLevel::NotSupported);
    let path = u.path().trim_matches('/');
    if !path.is_empty() {
        cfg.database(path);
    }
    let s = tokio::net::TcpStream::connect((u.host_str().unwrap(), u.port().unwrap_or(1433)))
        .await
        .unwrap();
    tiberius::Client::connect(cfg, s.compat_write())
        .await
        .unwrap()
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// `[connections.erp]` for the database, with its password in a secret variable.
/// With `insecure`, no TLS: the databases of the tests have none, or a certificate of their own.
fn toml(d: &Db, insecure: bool) -> String {
    format!("[project]\nname = \"d\"\n\n[vars]\nERP_PASSWORD = {{ secret = true }}\n\n[connections.erp]\nkind = \"{}\"\nurl = \"{}\"\nuser = \"{}\"\npassword = {{ var = \"ERP_PASSWORD\" }}\ninsecure = {insecure}\ntimeout = \"20s\"\n", d.kind, d.url, d.user)
}

fn env(d: &Db) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("ERP_PASSWORD".to_string(), d.password.clone());
    env
}

/// A job for the connection `erp` of the database, run here.
fn job(d: &Db, job: impl FnOnce(Job0) -> Job) -> Outcome {
    job_with(d, &env(d), true, job)
}

fn job_with(
    d: &Db,
    env: &BTreeMap<String, String>,
    insecure: bool,
    job: impl FnOnce(Job0) -> Job,
) -> Outcome {
    let m = quadra_sema::manifest::parse(&toml(d, insecure)).unwrap();
    let conn = Job0::of(&m, "erp").unwrap();
    let j = job(conn);
    block_on(run(&j, env, false))
}

fn fetch(d: &Db, table: &str, column: &str, after: Option<&str>) -> Result<Vec<Item>, Failed> {
    match job(d, |conn| Job::Fetch {
        conn,
        ask: Ask::Rows {
            table: table.into(),
            column: Some(column.into()),
            after: after.map(|a| a.to_string()),
            limit: 1000,
        },
    }) {
        Outcome::Fetched(f) => Ok(f.items),
        Outcome::Failed(f) => Err(f),
        Outcome::Put => panic!("a fetch answered a put"),
    }
}

fn put(d: &Db, table: &str, row: J, key: &str) -> Result<(), Failed> {
    match job(d, |conn| Job::Put {
        conn,
        put: Put::Row {
            table: table.into(),
            row,
            key: key.into(),
        },
    }) {
        Outcome::Put => Ok(()),
        Outcome::Failed(f) => Err(f),
        Outcome::Fetched(_) => panic!("a put answered a fetch"),
    }
}

fn body(i: &Item) -> J {
    serde_json::from_slice(&i.body).unwrap()
}

/// All the rows of a table as the engine takes them: pull after pull from the cursor, each
/// row once (its name and its body), until a pull brings nothing new.
fn pull_all(d: &Db, table: &str, column: &str) -> (Vec<J>, usize) {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let mut after: Option<String> = None;
    let mut pulls = 0;
    loop {
        let items = fetch(d, table, column, after.as_deref()).unwrap();
        pulls += 1;
        let mut new = 0;
        for i in &items {
            if seen.insert((i.name.clone(), i.body.clone())) {
                rows.push(body(i));
                new += 1;
            }
        }
        if let Some(c) = items.last().and_then(|i| i.cursor.clone()) {
            after = Some(c);
        }
        if new == 0 {
            return (rows, pulls);
        }
    }
}

// ---- reading -------------------------------------------------------------------------------

/// Rows sharing a value of the cursor: more than a page of them, a few at a page's edge, and
/// one written later with the last value. None is lost, none comes twice.
fn ties(d: &Db) {
    let t = unique("ties");
    let ts = match dialect(d) {
        "postgres" => "TIMESTAMP",
        "mysql" => "DATETIME",
        _ => "DATETIME2",
    };
    let ins = |from: i64, to: i64, at: &str| {
        let values: Vec<String> = (from..=to).map(|i| format!("({i}, '{at}')")).collect();
        values
            .chunks(500)
            .map(|c| format!("INSERT INTO {t} (id, changed) VALUES {}", c.join(", ")))
            .collect::<Vec<_>>()
    };
    let mut stmts = vec![
        format!("DROP TABLE IF EXISTS {t}"),
        format!("CREATE TABLE {t} (id INT PRIMARY KEY, changed {ts} NOT NULL)"),
    ];
    // 2500 rows of one instant: no page can move past it alone
    stmts.extend(ins(1, 2500, "2026-11-27 10:00:00"));
    // then 999 + 3 rows: three of them at the edge of the next page
    for i in 0..999 {
        stmts.push(format!(
            "INSERT INTO {t} (id, changed) VALUES ({}, '2026-11-27 10:{:02}:{:02}')",
            2501 + i,
            1 + i / 60,
            i % 60
        ));
    }
    stmts.extend(ins(3500, 3502, "2026-11-27 11:00:00"));
    exec(d, &stmts);
    let (rows, pulls) = pull_all(d, &t, "changed");
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    let unique_ids: HashSet<i64> = ids.iter().copied().collect();
    assert_eq!(ids.len(), 2500 + 999 + 3, "{} pulls", pulls);
    assert_eq!(unique_ids.len(), ids.len());
    // a row written afterwards with the last value is not lost (`>=`, not `>`)
    exec(
        d,
        &[format!(
            "INSERT INTO {t} (id, changed) VALUES (9999, '2026-11-27 11:00:00')"
        )],
    );
    let last = rows.last().unwrap()["changed"]
        .as_str()
        .unwrap()
        .to_string();
    let items = fetch(d, &t, "changed", Some(&last)).unwrap();
    assert!(
        items.iter().any(|i| body(i)["id"] == json!(9999)),
        "{:?}",
        items.iter().map(body).collect::<Vec<_>>()
    );
    exec(d, &[format!("DROP TABLE {t}")]);
}

#[test]
fn rows_of_the_same_cursor_value_come_once_each_postgres() {
    if let Some(d) = pg() {
        ties(&d);
    }
}

#[test]
fn rows_of_the_same_cursor_value_come_once_each_mysql() {
    if let Some(d) = mysql() {
        ties(&d);
    }
}

#[test]
fn rows_of_the_same_cursor_value_come_once_each_sqlserver() {
    if let Some(d) = mssql() {
        ties(&d);
    }
}

/// The values of a row as JSON, exactly: decimals and big integers as text, instants as ISO
/// text, binary as base64; and a numeric cursor that does not sort as text.
fn values(d: &Db, ddl: &str, insert: &str, expect: J) {
    let t = unique("vals");
    exec(
        d,
        &[
            format!("DROP TABLE IF EXISTS {t}"),
            format!("CREATE TABLE {t} ({ddl})"),
            format!("INSERT INTO {t} {insert}"),
        ],
    );
    let items = fetch(d, &t, "id", None).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(body(&items[0]), expect, "{}", d.kind);
    assert_eq!(items[0].cursor.as_deref(), Some("9"));
    assert_eq!(items[1].cursor.as_deref(), Some("10"));
    assert_eq!(items[0].name, format!("{t}/9"));
    // from the cursor `9`: 9 again (`>=`) and 10, compared as numbers
    let again = fetch(d, &t, "id", Some("9")).unwrap();
    assert_eq!(again.len(), 2);
    let after = fetch(d, &t, "id", Some("10")).unwrap();
    assert_eq!(after.len(), 1);
    exec(d, &[format!("DROP TABLE {t}")]);
}

#[cfg(feature = "odbc")]
#[test]
fn odbc_reads_in_pieces_and_writes_once() {
    let Some(d) = odbc() else { return };
    ties(&d);
    exchange(&d);
    let t = unique("odbcvals");
    exec(
        &d,
        &[
            format!("DROP TABLE IF EXISTS {t}"),
            format!("CREATE TABLE {t} (id INT PRIMARY KEY, amount NUMERIC(20,4), at TIMESTAMP, note TEXT)"),
            format!("INSERT INTO {t} VALUES (9, 1234567890123456.7891, '2026-11-27 10:14:03.5', 'città'), (10, NULL, NULL, NULL)"),
        ],
    );
    let items = fetch(&d, &t, "id", Some("9")).unwrap();
    assert_eq!(items.len(), 2);
    let row = body(&items[0]);
    assert_eq!(row["id"], json!(9));
    assert_eq!(row["amount"], json!("1234567890123456.7891"));
    assert_eq!(row["at"], json!("2026-11-27T10:14:03.5"));
    assert_eq!(row["note"], json!("città"));
    assert_eq!(body(&items[1])["amount"], J::Null);
    exec(&d, &[format!("DROP TABLE {t}")]);
}

#[test]
fn values_come_out_exactly_postgres() {
    let Some(d) = pg() else { return };
    values(
        &d,
        "id INT PRIMARY KEY, amount NUMERIC(20,4), big BIGINT, small SMALLINT, ratio DOUBLE PRECISION, ok BOOLEAN, day DATE, at TIMESTAMP, atz TIMESTAMPTZ, note TEXT, code CHAR(3), raw BYTEA, doc JSONB, uid UUID, missing TEXT",
        "VALUES (9, 1234567890123456.7891, 9007199254740993, -3, 0.5, true, '2026-11-27', '2026-11-27 10:14:03.123456', '2026-11-27 11:14:03+01', 'città', 'AB', '\\x00ff10', '{\"a\": [1, 2]}', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', NULL), (10, 0, 1, 1, 1, false, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        json!({
            "id": 9, "amount": "1234567890123456.7891", "big": "9007199254740993", "small": -3,
            "ratio": 0.5, "ok": true, "day": "2026-11-27", "at": "2026-11-27T10:14:03.123456",
            "atz": "2026-11-27T10:14:03Z", "note": "città", "code": "AB ", "raw": "AP8Q",
            "doc": {"a": [1, 2]}, "uid": "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", "missing": null
        }),
    );
}

#[test]
fn values_come_out_exactly_mysql() {
    let Some(d) = mysql() else { return };
    values(
        &d,
        "id INT PRIMARY KEY, amount DECIMAL(20,4), big BIGINT UNSIGNED, small SMALLINT, ratio DOUBLE, ok BOOLEAN, day DATE, at DATETIME(6), atz TIMESTAMP(3) NULL, note VARCHAR(20), raw VARBINARY(10), doc JSON, missing TEXT",
        "VALUES (9, 1234567890123456.7891, 18446744073709551615, -3, 0.5, true, '2026-11-27', '2026-11-27 10:14:03.123456', '2026-11-27 10:14:03.500', 'città', X'00ff10', '{\"a\": [1, 2]}', NULL), (10, 0, 1, 1, 1, false, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        json!({
            "id": 9, "amount": "1234567890123456.7891", "big": "18446744073709551615", "small": -3,
            "ratio": 0.5, "ok": 1, "day": "2026-11-27", "at": "2026-11-27T10:14:03.123456",
            "atz": "2026-11-27T10:14:03.5Z", "note": "città", "raw": "AP8Q",
            "doc": {"a": [1, 2]}, "missing": null
        }),
    );
}

#[test]
fn values_come_out_exactly_sqlserver() {
    let Some(d) = mssql() else { return };
    values(
        &d,
        "id INT PRIMARY KEY, amount DECIMAL(20,4), big BIGINT, small SMALLINT, ratio FLOAT, ok BIT, day DATE, at DATETIME2(7), old DATETIME, atz DATETIMEOFFSET, note NVARCHAR(20), raw VARBINARY(10), uid UNIQUEIDENTIFIER, missing NVARCHAR(10)",
        "VALUES (9, 1234567890123456.7891, 9007199254740993, -3, 0.5, 1, '2026-11-27', '2026-11-27T10:14:03.1234567', '2026-11-27T10:14:03.123', '2026-11-27T11:14:03+01:00', N'città', 0x00ff10, 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', NULL), (10, 0, 1, 1, 1, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        json!({
            "id": 9, "amount": "1234567890123456.7891", "big": "9007199254740993", "small": -3,
            "ratio": 0.5, "ok": true, "day": "2026-11-27", "at": "2026-11-27T10:14:03.1234567",
            "old": "2026-11-27T10:14:03.123", "atz": "2026-11-27T11:14:03+01:00", "note": "città",
            "raw": "AP8Q", "uid": "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11", "missing": null
        }),
    );
}

// ---- writing -------------------------------------------------------------------------------

/// A row into an exchange table, sent twice with the same key: one row, its decimal exact; a
/// table without `quadra_key` takes it as it is; a column it does not have is refused.
fn exchange(d: &Db) {
    let t = unique("exchange");
    let plain = unique("plain");
    let (varchar, key) = match dialect(d) {
        "sqlserver" => ("NVARCHAR(40)", "NVARCHAR(100)"),
        _ => ("VARCHAR(40)", "VARCHAR(100)"),
    };
    let ts = match dialect(d) {
        "postgres" => "TIMESTAMP",
        "mysql" => "DATETIME",
        _ => "DATETIME2",
    };
    exec(
        d,
        &[
            format!("DROP TABLE IF EXISTS {t}"),
            format!("DROP TABLE IF EXISTS {plain}"),
            format!("CREATE TABLE {t} (number {varchar} NOT NULL, total DECIMAL(18,2), placed {ts}, urgent SMALLINT, quadra_key {key} UNIQUE)"),
            format!("CREATE TABLE {plain} (number {varchar}, total DECIMAL(18,2))"),
        ],
    );
    let row = json!({"number": "A-1", "total": "1234567890123456.78", "placed": "2026-11-27T10:14:03Z", "urgent": true});
    put(d, &t, row.clone(), "quadra-1-41").unwrap();
    // the same event again (an answer lost, a restart): not written twice
    put(d, &t, row.clone(), "quadra-1-41").unwrap();
    put(
        d,
        &t,
        json!({"number": "A-2", "total": null}),
        "quadra-1-42",
    )
    .unwrap();
    assert_eq!(
        query(d, &format!("SELECT number FROM {t} ORDER BY number")),
        ["A-1", "A-2"]
    );
    assert_eq!(
        query(
            d,
            &format!("SELECT {} FROM {t} WHERE number = 'A-1'", text(d, "total"))
        ),
        ["1234567890123456.78"]
    );
    assert_eq!(
        query(d, &format!("SELECT quadra_key FROM {t} ORDER BY number")),
        ["quadra-1-41", "quadra-1-42"]
    );
    let placed = query(
        d,
        &format!("SELECT {} FROM {t} WHERE number = 'A-1'", text(d, "placed")),
    );
    assert!(
        placed[0].starts_with("2026-11-27") && placed[0].contains("10:14:03"),
        "{placed:?}"
    );
    // without quadra_key: as it is
    put(d, &plain, json!({"number": "B-1", "total": "0.10"}), "k").unwrap();
    assert_eq!(
        query(d, &format!("SELECT {} FROM {plain}", text(d, "total"))),
        ["0.10"]
    );
    // a column the table does not have, a table that is not there: refused for good
    let Err(Failed::Never(e)) = put(d, &t, json!({"numbr": "A-3"}), "quadra-1-43") else {
        panic!()
    };
    assert!(
        e.contains("no column `numbr`") && e.contains("number"),
        "{e}"
    );
    let Err(Failed::Never(e)) = put(d, &unique("none"), json!({"a": 1}), "k") else {
        panic!()
    };
    assert!(!e.contains(&d.password), "{e}");
    exec(
        d,
        &[format!("DROP TABLE {t}"), format!("DROP TABLE {plain}")],
    );
}

#[test]
fn an_exchange_row_is_written_once_per_event_postgres() {
    if let Some(d) = pg() {
        exchange(&d);
    }
}

#[test]
fn an_exchange_row_is_written_once_per_event_mysql() {
    if let Some(d) = mysql() {
        exchange(&d);
    }
}

#[test]
fn an_exchange_row_is_written_once_per_event_sqlserver() {
    if let Some(d) = mssql() {
        exchange(&d);
    }
}

/// A wrong password is refused for good, without the password in the message; a database
/// that does not answer is tried again.
fn refusals(d: &Db) {
    let mut wrong = env(d);
    wrong.insert("ERP_PASSWORD".into(), "not-the-password-0451".into());
    let ask = |conn| Job::Fetch {
        conn,
        ask: Ask::Rows {
            table: "orders".into(),
            column: Some("id".into()),
            after: None,
            limit: 10,
        },
    };
    let Outcome::Failed(Failed::Never(e)) = job_with(d, &wrong, true, ask) else {
        panic!()
    };
    assert!(
        !e.contains("not-the-password-0451") && !e.contains(&d.password),
        "{e}"
    );
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let gone = Db {
        kind: d.kind,
        url: format!("{}://127.0.0.1:{port}/erp", d.kind),
        user: d.user.clone(),
        password: d.password.clone(),
        full: String::new(),
    };
    let Outcome::Failed(Failed::Again(e)) = job_with(&gone, &env(d), true, ask) else {
        panic!()
    };
    assert!(e.contains("cannot connect"), "{e}");
    // with TLS, a database without it or with a certificate no authority signed is refused
    // for good, saying what to do
    let Outcome::Failed(Failed::Never(e)) = job_with(d, &env(d), false, ask) else {
        panic!("{}: TLS", d.kind)
    };
    eprintln!("{}: {e}", d.kind);
    assert!(e.contains("insecure = true"), "{e}");
}

#[test]
fn credentials_refused_fail_for_good_and_silence_is_tried_again() {
    for d in [pg(), mysql(), mssql()].into_iter().flatten() {
        refusals(&d);
    }
}

// ---- a host: a large table, interrupted ----------------------------------------------------

struct One(TenantSpec);

impl TenantResolver for One {
    fn resolve(&self, _: &str) -> Option<TenantSpec> {
        Some(self.0.clone())
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-db-{name}-{}", std::process::id()))
}

/// A project with the connection `erp` to the database and `code`, its own data initialized.
fn project(dir: &Path, d: &Db, code: &str) -> (Config, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), toml(d, true)).unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    let now = chrono::Local::now().naive_local();
    ops::init(&url, checked.model.as_ref().unwrap(), &checked.report, now).unwrap();
    let (token, _) = auth::create(
        &mut Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now,
        None,
    )
    .unwrap();
    let cfg = Config {
        outside: Outside {
            env: env(d),
            deliver: true,
        },
        root: dir.to_path_buf(),
        url,
        port: 0,
        dev_login: false,
        secure_cookies: false,
        cache_mb: 64,
        prewarm_days: None,
        web: None,
        sweep_every: 0,
        editors: vec![],
        watch: false,
        limits: Limits::default(),
        engines: Default::default(),
    };
    (cfg, token)
}

/// A host of the project, started (its pulls running) and serving on a port of its own, on a
/// runtime of its own.
fn start(cfg: &Config) -> (Host, tokio::runtime::Runtime, u16) {
    let host = Host::new(
        HostOptions::default(),
        Arc::new(One(TenantSpec {
            id: "d".into(),
            config: cfg.clone(),
        })),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let h = host.clone();
    rt.spawn(async move {
        h.serve(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .unwrap()
    });
    rt.block_on(host.start("d.test")).unwrap();
    (host, rt, port)
}

/// A transaction through the HTTP API.
fn tx(port: u16, token: &str, ops: J) {
    use std::io::{Read as _, Write as _};
    static KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body = json!({ "ops": ops }).to_string();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!("POST /v1/tx HTTP/1.1\r\nHost: d.test\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: tx-{key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
}

fn count(cfg: &Config, sql: &str) -> i64 {
    Conn::open(&cfg.url)
        .unwrap()
        .query(sql, &[])
        .unwrap()
        .first()
        .and_then(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn pull_error(cfg: &Config) -> Option<String> {
    Conn::open(&cfg.url)
        .unwrap()
        .query("SELECT \"error\" FROM \"_quadra_cursors\"", &[])
        .unwrap()
        .first()
        .and_then(|r| r[0].clone())
}

const RECEIVED: &str = "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_inbox\"";
const HANDLED: &str =
    "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_inbox\" WHERE \"state\" = 'done'";

#[test]
fn a_large_table_comes_in_in_pieces_and_resumes_after_an_interruption() {
    let Some(d) = pg() else { return };
    let n: i64 = std::env::var("QUADRA_TEST_DB_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);
    let schema = unique("big");
    // three rows per instant: ties at the edges of the pages
    exec(
        &d,
        &[
            format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
            format!("CREATE SCHEMA {schema}"),
            format!("CREATE TABLE {schema}.orders (id BIGINT PRIMARY KEY, number TEXT NOT NULL, total NUMERIC(12,2) NOT NULL, updated_at TIMESTAMP NOT NULL)"),
            format!("INSERT INTO {schema}.orders SELECT g, 'N' || g, (g % 100000) / 100.0 + 0.01, TIMESTAMP '2026-01-01' + (g / 3) * INTERVAL '1 second' FROM generate_series(1, {n}) g"),
            format!("CREATE INDEX ON {schema}.orders (updated_at)"),
        ],
    );
    let expected_total = query(
        &d,
        &format!("SELECT CAST(SUM(total) AS TEXT) FROM {schema}.orders"),
    )[0]
    .clone();

    // the connection alone: the whole table, a page at a time from the cursor
    let began = Instant::now();
    let (mut ids, mut after, mut pages) = (HashSet::new(), None::<String>, 0);
    loop {
        let items = fetch(
            &d,
            &format!("{schema}.orders"),
            "updated_at",
            after.as_deref(),
        )
        .unwrap();
        pages += 1;
        let before = ids.len();
        for i in &items {
            ids.insert(body(i)["id"].as_i64().unwrap());
        }
        after = items.last().and_then(|i| i.cursor.clone());
        if ids.len() == before {
            break;
        }
    }
    assert_eq!(ids.len() as i64, n);
    eprintln!(
        "{n} rows read by the connection in {pages} pages in {:.1} s",
        began.elapsed().as_secs_f64()
    );
    drop(ids);
    let code = format!("record ErpOrder from json:\n    id: Int\n    number: Text\n    total: Decimal\n    updated_at: DateTime\n\nentity Order:\n    number: Text\n    total: Decimal\n\ninbound OrderIn via erp as m: ErpOrder:\n    pull \"{schema}.orders\" every 1 minute\n    cursor m.updated_at\n    let o = Order(number=m.number, total=m.total)\n");
    let dir = temp("large");
    let (cfg, _) = project(&dir, &d, &code);

    let began = Instant::now();
    let (host, rt, _) = start(&cfg);
    // interrupted a third of the way in
    while count(&cfg, RECEIVED) < n / 3 {
        assert!(
            began.elapsed() < Duration::from_secs(1800),
            "{:?}",
            pull_error(&cfg)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    rt.block_on(host.stop("d"));
    drop(host);
    rt.shutdown_timeout(Duration::from_secs(10));
    let (received, handled) = (count(&cfg, RECEIVED), count(&cfg, HANDLED));
    let cursor = Conn::open(&cfg.url)
        .unwrap()
        .query("SELECT \"cursor\" FROM \"_quadra_cursors\"", &[])
        .unwrap()[0][0]
        .clone();
    eprintln!(
        "interrupted after {:.1} s: {received} rows received, {handled} handled, cursor {cursor:?}",
        began.elapsed().as_secs_f64()
    );
    assert!(received < n, "the interruption came too late to tell");

    let resumed = Instant::now();
    let (host, rt, _) = start(&cfg);
    let mut all_in = None;
    loop {
        let (r, h) = (count(&cfg, RECEIVED), count(&cfg, HANDLED));
        if r >= n && all_in.is_none() {
            all_in = Some(began.elapsed());
        }
        if h >= n {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(3600),
            "{r} received, {h} handled: {:?}",
            pull_error(&cfg)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let done = began.elapsed();
    // nothing more comes in
    std::thread::sleep(Duration::from_secs(2));
    rt.block_on(host.stop("d"));
    drop(host);
    rt.shutdown_timeout(Duration::from_secs(10));
    eprintln!(
        "{n} rows: all received {:.1} s after the start, all handled after {:.1} s ({:.0} rows/s; resumed for {:.1} s)",
        all_in.unwrap_or(done).as_secs_f64(),
        done.as_secs_f64(),
        n as f64 / done.as_secs_f64(),
        resumed.elapsed().as_secs_f64()
    );
    // each row once: no duplicates, no gaps
    assert_eq!(count(&cfg, RECEIVED), n);
    assert_eq!(count(&cfg, HANDLED), n);
    assert_eq!(
        count(&cfg, "SELECT CAST(COUNT(*) AS TEXT) FROM \"order\""),
        n
    );
    assert_eq!(
        count(
            &cfg,
            "SELECT CAST(COUNT(DISTINCT \"number\") AS TEXT) FROM \"order\""
        ),
        n
    );
    let total = Conn::open(&cfg.url)
        .unwrap()
        .query("SELECT \"total\" FROM \"order\"", &[])
        .unwrap()
        .into_iter()
        .map(|r| {
            r[0].as_deref()
                .unwrap()
                .parse::<rust_decimal::Decimal>()
                .unwrap()
        })
        .sum::<rust_decimal::Decimal>();
    assert_eq!(total.to_string(), expected_total);
    assert_eq!(pull_error(&cfg), None);
    exec(&d, &[format!("DROP SCHEMA {schema} CASCADE")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_field_without_its_column_stops_the_pull_with_why() {
    let Some(d) = pg() else { return };
    let t = unique("cols");
    exec(
        &d,
        &[
            format!("DROP TABLE IF EXISTS {t}"),
            format!("CREATE TABLE {t} (id INT PRIMARY KEY, number TEXT)"),
            format!("INSERT INTO {t} VALUES (1, 'A')"),
        ],
    );
    let code = format!("record ErpOrder from json:\n    id: Int\n    number: Text at \"NUMBER\"\n\nentity Order:\n    number: Text\n\ninbound OrderIn via erp as m: ErpOrder:\n    pull \"{t}\" every 1 minute\n    cursor m.id\n    let o = Order(number=m.number)\n");
    let dir = temp("cols");
    let (cfg, _) = project(&dir, &d, &code);
    let (host, rt, _) = start(&cfg);
    let began = Instant::now();
    while pull_error(&cfg).is_none() && began.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    rt.block_on(host.stop("d"));
    let e = pull_error(&cfg).unwrap_or_default();
    assert!(
        e.contains("no column `NUMBER`")
            && e.contains("`number` of ErpOrder")
            && e.contains("id, number"),
        "{e}"
    );
    assert_eq!(count(&cfg, RECEIVED), 0);
    exec(&d, &[format!("DROP TABLE {t}")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_outbound_inserts_its_row_into_the_exchange_table() {
    let Some(d) = pg() else { return };
    let t = unique("outq");
    exec(
        &d,
        &[
            format!("DROP TABLE IF EXISTS {t}"),
            format!("CREATE TABLE {t} (\"NUMBER\" TEXT, \"TOTAL\" NUMERIC(14,2), quadra_key TEXT UNIQUE)"),
        ],
    );
    let code = format!("entity Order:\n    number: Text\n    total: Decimal\n\nevent OrderPlaced(number: Text, total: Decimal)\n\non Order created as o:\n    emit OrderPlaced(number=o.number, total=o.total)\n\nrecord ErpRow from json:\n    number: Text at \"NUMBER\"\n    total: Decimal at \"TOTAL\"\n\noutbound OrderPlaced via erp as e:\n    insert(\"{t}\", ErpRow(number=e.number, total=e.total))\n");
    let dir = temp("outbound");
    let (cfg, token) = project(&dir, &d, &code);
    let (host, rt, port) = start(&cfg);
    for (number, total) in [("A-1", "12345678901.10"), ("A-2", "0.05")] {
        tx(
            port,
            &token,
            json!([{"op": "create", "entity": "Order", "values": {"number": number, "total": total}}]),
        );
    }
    let began = Instant::now();
    while query(&d, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM {t}"))[0] != "2" {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "{:?}",
            Conn::open(&cfg.url)
                .unwrap()
                .query("SELECT \"error\" FROM \"_quadra_outbox\"", &[])
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    rt.block_on(host.stop("d"));
    assert_eq!(
        query(
            &d,
            &format!("SELECT CAST(\"TOTAL\" AS TEXT) FROM {t} ORDER BY \"NUMBER\"")
        ),
        ["12345678901.10", "0.05"]
    );
    let keys = query(&d, &format!("SELECT quadra_key FROM {t}"));
    assert!(keys.iter().all(|k| k.starts_with("quadra-")), "{keys:?}");
    exec(&d, &[format!("DROP TABLE {t}")]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the compiler ----------------------------------------------------------------------------

/// What the compiler says of the code for a database: the messages of the project.
fn diagnostics(code: &str) -> Vec<String> {
    let dir = temp("check");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        "[project]\nname = \"d\"\n\n[vars]\nERP_PASSWORD = { secret = true }\n\n[connections.erp]\nkind = \"postgres\"\nurl = \"postgres://db.example.com/erp\"\nuser = \"quadra\"\npassword = { var = \"ERP_PASSWORD\" }\n\n[connections.hooks]\nkind = \"http\"\nurl = \"https://hooks.example.com\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    let checked = quadra_analysis::check(&Project::load(&dir).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == quadra_sema::Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn the_compiler_wants_a_column_for_the_cursor_and_a_json_row_for_insert() {
    let base = "entity Order:\n    number: Text\n\nrecord Row from json:\n    number: Text at \"ORDER_NO\"\n    changed: DateTime at \"UPDATED_AT\"\n\nrecord Line from csv:\n    number: Text\n\nevent OrderPlaced(number: Text)\n\n";
    // fine: a column, a record from json, an exchange table
    let ok = format!("{base}inbound OrderIn via erp as m: Row:\n    pull \"erp.orders\" every 5 minutes\n    cursor m.changed\n    let o = Order(number=m.number)\n\noutbound OrderPlaced via erp as e:\n    insert(\"erp.exchange_orders\", Row(number=e.number, changed=now()))\n");
    assert_eq!(diagnostics(&ok), Vec::<String>::new());
    // the cursor is not a column
    let e = diagnostics(&format!("{base}inbound OrderIn via erp as m: Row:\n    pull \"erp.orders\" every 5 minutes\n    cursor m.number + \"x\"\n    let o = Order(number=m.number)\n"));
    assert!(
        e.iter()
            .any(|e| e.contains("the cursor of a table is one of its columns")),
        "{e:?}"
    );
    // no cursor
    let e = diagnostics(&format!("{base}inbound OrderIn via erp as m: Row:\n    pull \"erp.orders\" every 5 minutes\n    let o = Order(number=m.number)\n"));
    assert!(
        e.iter().any(|e| e.contains("say the column with `cursor")),
        "{e:?}"
    );
    // a table that is not a name, a row that is not from json
    let e = diagnostics(&format!("{base}outbound OrderPlaced via erp as e:\n    insert(\"orders; drop table x\", Line(number=e.number))\n"));
    assert!(
        e.iter().any(|e| e.contains("a table, its name only")),
        "{e:?}"
    );
    assert!(
        e.iter()
            .any(|e| e.contains("the row is a record `from json`")),
        "{e:?}"
    );
    // insert only into a database, a database only by insert
    let e = diagnostics(&format!("{base}outbound OrderPlaced via hooks as e:\n    insert(\"orders\", Row(number=e.number, changed=now()))\n"));
    assert!(!e.is_empty(), "insert through http");
    let e = diagnostics(&format!("{base}outbound OrderPlaced via erp as e:\n    post(\"/orders\", Row(number=e.number, changed=now()))\n"));
    assert!(
        e.iter().any(|e| e.contains("insert(table, record)")),
        "{e:?}"
    );
}
