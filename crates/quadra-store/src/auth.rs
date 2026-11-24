//! API credentials: tokens for programs and sessions for the web client. Only a SHA-256 of
//! the secret is stored; the secret is shown once. Sessions carry a CSRF token.

use crate::conn::{Conn, Sql};
use chrono::NaiveDateTime;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: u64,
    pub read_only: bool,
    /// `token` or `session`
    pub kind: String,
    pub csrf: Option<String>,
    pub name: String,
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system random generator");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn ts(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// A new credential; returns (secret, csrf). The secret is not stored.
pub fn create(conn: &mut Conn, kind: &str, user_id: u64, name: &str, read_only: bool, now: NaiveDateTime, expires: Option<NaiveDateTime>) -> Result<(String, Option<String>), String> {
    let secret = format!("qd_{}_{}", if kind == "session" { "s" } else { "t" }, random_hex(32));
    let csrf = (kind == "session").then(|| random_hex(16));
    conn.exec(
        "INSERT INTO \"_quadra_tokens\" (\"hash\", \"kind\", \"user_id\", \"name\", \"read_only\", \"csrf\", \"created_at\", \"expires_at\", \"revoked\") VALUES (?text, ?text, ?bigint, ?text, ?boolean, ?text, ?text, ?text, ?boolean)",
        &[
            Sql::Text(sha256_hex(&secret)),
            Sql::Text(kind.into()),
            Sql::Int(user_id as i64),
            Sql::Text(name.into()),
            Sql::Bool(read_only),
            csrf.clone().map(Sql::Text).unwrap_or(Sql::Null),
            Sql::Text(ts(now)),
            expires.map(|e| Sql::Text(ts(e))).unwrap_or(Sql::Null),
            Sql::Bool(false),
        ],
    )?;
    Ok((secret, csrf))
}

/// The principal behind a secret, if valid now.
pub fn authenticate(conn: &mut Conn, secret: &str, now: NaiveDateTime) -> Result<Option<Principal>, String> {
    if !secret.starts_with("qd_") || secret.len() > 200 {
        return Ok(None);
    }
    let rows = conn.query(
        "SELECT CAST(\"user_id\" AS TEXT), CAST(\"read_only\" AS TEXT), \"kind\", \"csrf\", \"expires_at\", CAST(\"revoked\" AS TEXT), \"name\" FROM \"_quadra_tokens\" WHERE \"hash\" = ?text",
        &[Sql::Text(sha256_hex(secret))],
    )?;
    let Some(r) = rows.first() else {
        return Ok(None);
    };
    let truthy = |s: &Option<String>| matches!(s.as_deref(), Some("1") | Some("true") | Some("t"));
    if truthy(&r[5]) {
        return Ok(None);
    }
    if let Some(e) = r[4].as_deref().and_then(|e| NaiveDateTime::parse_from_str(e, "%Y-%m-%dT%H:%M:%S").ok()) {
        if e < now {
            return Ok(None);
        }
    }
    Ok(Some(Principal { user_id: r[0].as_deref().and_then(|u| u.parse().ok()).unwrap_or(0), read_only: truthy(&r[1]), kind: r[2].clone().unwrap_or_default(), csrf: r[3].clone(), name: r[6].clone().unwrap_or_default() }))
}

/// Revoke credentials by name (tokens) or by secret (logout). Returns how many.
pub fn revoke(conn: &mut Conn, name_or_secret: &str) -> Result<u64, String> {
    conn.exec("UPDATE \"_quadra_tokens\" SET \"revoked\" = ?boolean WHERE \"name\" = ?text OR \"hash\" = ?text", &[Sql::Bool(true), Sql::Text(name_or_secret.into()), Sql::Text(sha256_hex(name_or_secret))])
}

/// A request already committed with this idempotency key: (request hash, response, tx).
pub fn idempotent(conn: &mut Conn, user_id: u64, key: &str) -> Result<Option<(String, String, u64)>, String> {
    let rows =
        conn.query("SELECT \"request_hash\", \"response\", CAST(\"tx\" AS TEXT) FROM \"_quadra_idempotency\" WHERE \"user_id\" = ?bigint AND \"key\" = ?text", &[Sql::Int(user_id as i64), Sql::Text(key.into())])?;
    Ok(rows.first().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), r[2].as_deref().and_then(|t| t.parse().ok()).unwrap_or(0))))
}
