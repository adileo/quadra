//! The limits of a job in the database layer: a statement past the deadline stops, a
//! statement that returns more rows than the cap fails, and both are recorded for the engine.
//! PostgreSQL runs only when `QUADRA_TEST_PG` holds a url (`scripts/pg.sh test` sets it up).

use quadra_runtime::limits::{self, Limits};
use quadra_store::conn::{set_statement_timeout, Conn};
use std::time::{Duration, Instant};

/// Counts forever: only the time limit stops it.
const ENDLESS: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT CAST(COUNT(*) AS TEXT) FROM c";

fn sqlite() -> Conn {
    Conn::open("sqlite::memory:").unwrap()
}

#[test]
fn a_sqlite_statement_stops_at_the_deadline() {
    let mut c = sqlite();
    let limit = Duration::from_millis(300);
    let start = Instant::now();
    let _g = limits::begin(Limits { deadline: Some(start + limit), ..Default::default() });
    let e = c.query(ENDLESS, &[]).unwrap_err();
    let took = start.elapsed();
    assert!(e.starts_with("timeout"), "{e}");
    assert!(took < limit + limit / 10, "stopped after {took:?}");
    assert_eq!(limits::take_hit().as_deref(), Some(limits::TIMEOUT));
    // the connection serves the next statement of the next job
    drop(_g);
    let _g = limits::begin(Limits::default());
    assert_eq!(c.query("SELECT CAST(1 + 1 AS TEXT)", &[]).unwrap(), vec![vec![Some("2".to_string())]]);
}

#[test]
fn a_sqlite_statement_returning_too_many_rows_fails() {
    let mut c = sqlite();
    let _g = limits::begin(Limits { max_rows: Some(10), ..Default::default() });
    let ten = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 10) SELECT CAST(x AS TEXT) FROM c";
    assert_eq!(c.query(ten, &[]).unwrap().len(), 10);
    assert_eq!(limits::take_hit(), None, "exactly the cap is fine");
    // endless rows: reading stops one row past the cap, nothing else is kept
    let e = c.query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT CAST(x AS TEXT) FROM c", &[]).unwrap_err();
    assert!(e.starts_with("too many rows"), "{e}");
    assert!(limits::take_hit().is_some_and(|m| m.starts_with("too many rows")));
    // bookkeeping of the engine is not capped
    let n = limits::without_row_cap(|| c.query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 50) SELECT CAST(x AS TEXT) FROM c", &[]).unwrap().len());
    assert_eq!(n, 50);
}

#[test]
fn postgres_statements_stop_at_the_time_limit_and_the_row_cap() {
    let Ok(url) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped (run scripts/pg.sh test)");
        return;
    };
    let limit = Duration::from_millis(300);
    set_statement_timeout(Some(limit));
    let mut c = Conn::open(&url).unwrap();
    set_statement_timeout(None);
    let start = Instant::now();
    let _g = limits::begin(Limits { deadline: Some(start + limit), ..Default::default() });
    let e = c.query("SELECT CAST(pg_sleep(5) AS TEXT)", &[]).unwrap_err();
    let took = start.elapsed();
    assert!(e.starts_with("timeout"), "{e}");
    assert!(took < limit + limit / 10 + Duration::from_millis(50), "stopped after {took:?}");
    assert_eq!(limits::take_hit().as_deref(), Some(limits::TIMEOUT));
    assert_eq!(c.query("SELECT CAST(1 + 1 AS TEXT)", &[]).unwrap(), vec![vec![Some("2".to_string())]]);

    drop(_g);
    let _g = limits::begin(Limits { max_rows: Some(10), ..Default::default() });
    assert_eq!(c.query("SELECT CAST(x AS TEXT) FROM generate_series(1, 10) x", &[]).unwrap().len(), 10);
    let e = c.query("SELECT CAST(x AS TEXT) FROM generate_series(1, 1000000) x", &[]).unwrap_err();
    assert!(e.starts_with("too many rows"), "{e}");
    // the rest of the stream was dropped: the connection still works
    assert_eq!(c.query("SELECT CAST(3 AS TEXT)", &[]).unwrap(), vec![vec![Some("3".to_string())]]);
}
