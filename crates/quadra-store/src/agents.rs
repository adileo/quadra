//! The on-premise agents of a project (`quadra agent`), as its database keeps them: a name, the
//! hash of the one-time code that registers it, then the hash of the key it signs in with, and
//! when it was revoked. Neither the code nor the key is kept: only their SHA-256.

use crate::conn::{Conn, Sql};

/// An agent, as the studio lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub created_at: String,
    pub created_by: Option<String>,
    pub registered_at: Option<String>,
    pub revoked_at: Option<String>,
    /// waiting for its code to be used, until then
    pub code_expires: Option<String>,
    pub version: Option<String>,
    pub seen_at: Option<String>,
}

/// A one-time code: twelve letters and digits without the ones easy to confuse, in groups of
/// four (`K7QD-9XMP-2RTA`).
pub fn new_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).expect("system random generator");
    let s: String = b
        .iter()
        .map(|x| ALPHABET[(*x as usize) % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}-{}", &s[0..4], &s[4..8], &s[8..12])
}

/// The hex SHA-256 of a code or a key.
pub fn hash(secret: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The agent `name` added, or added again (a new code, its old key no longer valid, revoked no
/// more): its one-time code is `code`, valid until `expires`.
pub fn add(
    conn: &mut Conn,
    name: &str,
    code: &str,
    expires: &str,
    now: &str,
    by: &str,
) -> Result<(), String> {
    conn.exec(
        "DELETE FROM \"_quadra_agents\" WHERE \"name\" = ?text",
        &[Sql::Text(name.to_string())],
    )?;
    conn.exec(
        "INSERT INTO \"_quadra_agents\" (\"name\", \"code_hash\", \"code_expires\", \"created_at\", \"created_by\") VALUES (?text, ?text, ?text, ?text, ?text)",
        &[
            Sql::Text(name.to_string()),
            Sql::Text(hash(code)),
            Sql::Text(expires.to_string()),
            Sql::Text(now.to_string()),
            Sql::Text(by.to_string()),
        ],
    )?;
    Ok(())
}

/// The code `code` used by an agent (`version`): it is registered with the key `key`. Returns
/// its name; none when the code is unknown, used already or expired.
pub fn register(
    conn: &mut Conn,
    code: &str,
    key: &str,
    version: &str,
    now: &str,
) -> Result<Option<String>, String> {
    let rows = conn.query(
        "SELECT \"name\" FROM \"_quadra_agents\" WHERE \"code_hash\" = ?text AND \"code_expires\" >= ?text AND \"revoked_at\" IS NULL",
        &[Sql::Text(hash(code)), Sql::Text(now.to_string())],
    )?;
    let Some(name) = rows.into_iter().next().and_then(|r| r[0].clone()) else {
        return Ok(None);
    };
    conn.exec(
        "UPDATE \"_quadra_agents\" SET \"code_hash\" = NULL, \"code_expires\" = NULL, \"key_hash\" = ?text, \"registered_at\" = ?text, \"version\" = ?text, \"seen_at\" = ?text WHERE \"name\" = ?text",
        &[
            Sql::Text(hash(key)),
            Sql::Text(now.to_string()),
            Sql::Text(version.to_string()),
            Sql::Text(now.to_string()),
            Sql::Text(name.clone()),
        ],
    )?;
    Ok(Some(name))
}

/// The agent whose key is `key`, if it is registered and not revoked.
pub fn by_key(conn: &mut Conn, key: &str) -> Result<Option<String>, String> {
    Ok(conn
        .query(
            "SELECT \"name\" FROM \"_quadra_agents\" WHERE \"key_hash\" = ?text AND \"revoked_at\" IS NULL",
            &[Sql::Text(hash(key))],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].clone()))
}

/// The agent `name` revoked: its key and its code are no longer valid. Returns whether it was
/// there and not revoked already.
pub fn revoke(conn: &mut Conn, name: &str, now: &str) -> Result<bool, String> {
    Ok(conn.exec(
        "UPDATE \"_quadra_agents\" SET \"revoked_at\" = ?text, \"key_hash\" = NULL, \"code_hash\" = NULL WHERE \"name\" = ?text AND \"revoked_at\" IS NULL",
        &[Sql::Text(now.to_string()), Sql::Text(name.to_string())],
    )? > 0)
}

/// When the agent `name` (at `version`) was last seen: written at most once a minute by the
/// server, so that the studio shows it after a restart too.
pub fn seen(conn: &mut Conn, name: &str, version: &str, now: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_agents\" SET \"seen_at\" = ?text, \"version\" = ?text WHERE \"name\" = ?text",
        &[
            Sql::Text(now.to_string()),
            Sql::Text(version.to_string()),
            Sql::Text(name.to_string()),
        ],
    )?;
    Ok(())
}

/// The agents, by name.
pub fn list(conn: &mut Conn) -> Result<Vec<Agent>, String> {
    Ok(conn
        .query(
            "SELECT \"name\", \"created_at\", \"created_by\", \"registered_at\", \"revoked_at\", \"code_expires\", \"version\", \"seen_at\" FROM \"_quadra_agents\" ORDER BY \"name\"",
            &[],
        )?
        .into_iter()
        .map(|r| Agent {
            name: r[0].clone().unwrap_or_default(),
            created_at: r[1].clone().unwrap_or_default(),
            created_by: r[2].clone(),
            registered_at: r[3].clone(),
            revoked_at: r[4].clone(),
            code_expires: r[5].clone(),
            version: r[6].clone(),
            seen_at: r[7].clone(),
        })
        .collect())
}
