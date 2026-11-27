//! One writer per database. The sequencer of a serving project holds the database's writer
//! lock for as long as it runs; a command that writes past it (`quadra db migrate`, `quadra
//! change apply`, a fixture, an import, a sweep) takes the lock too, so it refuses to run while
//! a server serves the database instead of writing under it.
//!
//! SQLite: an exclusive lock on the file `<database>.writer` (released by the system if the
//! process dies). PostgreSQL: a session advisory lock keyed by the database's URL.

use crate::conn::Conn;
use sha2::{Digest, Sha256};

pub struct WriterLock {
    _file: Option<std::fs::File>,
    _conn: Option<Conn>,
}

fn busy() -> String {
    "a server is serving this database: stop it first, or make the change through it".into()
}

/// Takes the writer lock of the database at `url`, or says who holds it.
pub fn acquire(url: &str) -> Result<WriterLock, String> {
    if let Some(path) = url.strip_prefix("sqlite:") {
        let path = path.split('#').next().unwrap_or(path);
        if path.is_empty() || path == ":memory:" {
            return Ok(WriterLock {
                _file: None,
                _conn: None,
            });
        }
        let lock = format!("{path}.writer");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .map_err(|e| format!("{lock}: {e}"))?;
        return match f.try_lock() {
            Ok(()) => Ok(WriterLock {
                _file: Some(f),
                _conn: None,
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(busy()),
            Err(std::fs::TryLockError::Error(e)) => Err(format!("{lock}: {e}")),
        };
    }
    let mut conn = Conn::open(url)?;
    let key = i64::from_be_bytes(Sha256::digest(url.as_bytes())[..8].try_into().unwrap());
    let r = conn.query(
        "SELECT CAST(pg_try_advisory_lock(?bigint) AS TEXT)",
        &[crate::conn::Sql::Int(key)],
    )?;
    match r.first().and_then(|r| r[0].as_deref()) {
        Some("true") | Some("t") => Ok(WriterLock {
            _file: None,
            _conn: Some(conn),
        }),
        _ => Err(busy()),
    }
}
