//! The tokens of OAuth2 with a user's consent (`oauth2_grant = "authorization_code"`), one row
//! per connection in `_quadra_oauth`: the access and refresh tokens sealed by the server (the
//! store never sees them in the clear), and what may be shown of them: the scope, when the
//! access token expires, who connected it and when, the last error. Written through the
//! sequencer like the other system writes, so that they reach the tenant's log.

use crate::conn::{Conn, Sql};

/// The row of a connection's consent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grant {
    pub connection: String,
    /// the tokens, sealed (encrypted and authenticated) by the server
    pub sealed: Option<String>,
    pub scope: Option<String>,
    /// when the access token expires (`2026-11-27T10:14:03`, UTC); none: not said
    pub expires_at: Option<String>,
    pub connected_at: Option<String>,
    pub connected_by: Option<String>,
    pub refreshed_at: Option<String>,
    /// why the tokens no longer work (the refresh token revoked): an admin connects it again
    pub error: Option<String>,
}

const COLUMNS: &str = "\"connection\", \"sealed\", \"scope\", \"expires_at\", \"connected_at\", \"connected_by\", \"refreshed_at\", \"error\"";

fn of(r: Vec<Option<String>>) -> Grant {
    let mut r = r.into_iter();
    let mut next = || r.next().flatten();
    Grant {
        connection: next().unwrap_or_default(),
        sealed: next(),
        scope: next(),
        expires_at: next(),
        connected_at: next(),
        connected_by: next(),
        refreshed_at: next(),
        error: next(),
    }
}

/// The consent of `connection`, if it was given.
pub fn get(conn: &mut Conn, connection: &str) -> Result<Option<Grant>, String> {
    Ok(conn
        .query(
            &format!("SELECT {COLUMNS} FROM \"_quadra_oauth\" WHERE \"connection\" = ?text"),
            &[Sql::Text(connection.to_string())],
        )?
        .into_iter()
        .next()
        .map(of))
}

/// Every consent given, by connection.
pub fn all(conn: &mut Conn) -> Result<Vec<Grant>, String> {
    Ok(conn
        .query(
            &format!("SELECT {COLUMNS} FROM \"_quadra_oauth\" ORDER BY \"connection\""),
            &[],
        )?
        .into_iter()
        .map(of)
        .collect())
}

fn opt(v: &Option<String>) -> Sql {
    v.clone().map(Sql::Text).unwrap_or(Sql::Null)
}

/// A consent given (`connected_at`, `connected_by` set) or its tokens refreshed: the row as it
/// is now.
pub fn put(conn: &mut Conn, g: &Grant) -> Result<(), String> {
    conn.exec(
        "DELETE FROM \"_quadra_oauth\" WHERE \"connection\" = ?text",
        &[Sql::Text(g.connection.clone())],
    )?;
    conn.exec(
        &format!("INSERT INTO \"_quadra_oauth\" ({COLUMNS}) VALUES (?text, ?text, ?text, ?text, ?text, ?text, ?text, ?text)"),
        &[
            Sql::Text(g.connection.clone()),
            opt(&g.sealed),
            opt(&g.scope),
            opt(&g.expires_at),
            opt(&g.connected_at),
            opt(&g.connected_by),
            opt(&g.refreshed_at),
            opt(&g.error),
        ],
    )?;
    Ok(())
}

/// The consent of `connection` forgotten (disconnected). Whether there was one.
pub fn forget(conn: &mut Conn, connection: &str) -> Result<bool, String> {
    let had = get(conn, connection)?.is_some();
    conn.exec(
        "DELETE FROM \"_quadra_oauth\" WHERE \"connection\" = ?text",
        &[Sql::Text(connection.to_string())],
    )?;
    Ok(had)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_consent_is_kept_replaced_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("quadra-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite:{}", dir.join("d.db").display());
        let mut c = Conn::open(&url).unwrap();
        for s in crate::db::system_ddl_for_tests() {
            c.batch(&s).unwrap();
        }
        assert_eq!(get(&mut c, "shop").unwrap(), None);
        let mut g = Grant {
            connection: "shop".into(),
            sealed: Some("v1.abc".into()),
            scope: Some("read_orders".into()),
            connected_at: Some("2026-11-27T10:00:00".into()),
            connected_by: Some("user:1".into()),
            ..Default::default()
        };
        put(&mut c, &g).unwrap();
        g.sealed = Some("v1.def".into());
        g.refreshed_at = Some("2026-11-27T11:00:00".into());
        put(&mut c, &g).unwrap();
        assert_eq!(all(&mut c).unwrap(), vec![g.clone()]);
        assert!(forget(&mut c, "shop").unwrap());
        assert!(!forget(&mut c, "shop").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
