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

/// Revokes the credential of this secret, and only it (a logout): never by name, which anyone
/// could guess (`email code <login>`).
pub fn revoke_secret(conn: &mut Conn, secret: &str) -> Result<u64, String> {
    conn.exec("UPDATE \"_quadra_tokens\" SET \"revoked\" = ?boolean WHERE \"hash\" = ?text", &[Sql::Bool(true), Sql::Text(sha256_hex(secret))])
}

/// Revoke credentials by name (tokens, from the command line) or by secret. Returns how many.
pub fn revoke(conn: &mut Conn, name_or_secret: &str) -> Result<u64, String> {
    conn.exec("UPDATE \"_quadra_tokens\" SET \"revoked\" = ?boolean WHERE \"name\" = ?text OR \"hash\" = ?text", &[Sql::Bool(true), Sql::Text(name_or_secret.into()), Sql::Text(sha256_hex(name_or_secret))])
}

/// A request already committed with this idempotency key: (request hash, response, tx).
pub fn idempotent(conn: &mut Conn, user_id: u64, key: &str) -> Result<Option<(String, String, u64)>, String> {
    let rows =
        conn.query("SELECT \"request_hash\", \"response\", CAST(\"tx\" AS TEXT) FROM \"_quadra_idempotency\" WHERE \"user_id\" = ?bigint AND \"key\" = ?text", &[Sql::Int(user_id as i64), Sql::Text(key.into())])?;
    Ok(rows.first().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), r[2].as_deref().and_then(|t| t.parse().ok()).unwrap_or(0))))
}

// ---- sign-in with a code sent by email ----------------------------------------------------
//
// A code of 8 letters and digits (Crockford's base32: no I, L, O, U; 40 bits) for one user, valid
// for CODE_MINUTES, once, CODE_ATTEMPTS wrong tries at most; a new one replaces the user's
// open ones; CODES_PER_15_MIN per user. Only a hash is kept, salted with the request's id, which
// the browser holds: the code works only with the request it was sent for.

pub const CODE_MINUTES: i64 = 10;
pub const CODE_ATTEMPTS: i64 = 5;
pub const CODES_PER_15_MIN: i64 = 5;
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A code as a person types it: `7KQ3-M9XD`.
pub fn new_code() -> String {
    let mut out = String::new();
    let mut got = 0;
    while got < 8 {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("system random generator");
        for x in b {
            if got == 8 {
                break;
            }
            if x >= 224 {
                continue; // 224 = 7 × 32: every symbol equally likely
            }
            if got == 4 {
                out.push('-');
            }
            out.push(ALPHABET[(x % 32) as usize] as char);
            got += 1;
        }
    }
    out
}

/// What was typed, as it is compared: upper case, no separators, O as 0, I and L as 1.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

fn code_hash(id: &str, code: &str) -> String {
    sha256_hex(&format!("{id}:{}", normalize_code(code)))
}

/// A new code for a user: Ok((request id, code)), or Err(why not) when they asked too often.
pub fn code_start(conn: &mut Conn, user_id: u64, now: NaiveDateTime) -> Result<Result<(String, String), &'static str>, String> {
    let since = ts(now - chrono::Duration::minutes(15));
    let n: i64 = conn
        .query("SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_login_codes\" WHERE \"user_id\" = ?bigint AND \"created_at\" > ?text", &[Sql::Int(user_id as i64), Sql::Text(since)])?
        .first()
        .and_then(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    if n >= CODES_PER_15_MIN {
        return Ok(Err("too_many_codes"));
    }
    // the open ones are replaced
    conn.exec(
        "UPDATE \"_quadra_login_codes\" SET \"expires_at\" = ?text WHERE \"user_id\" = ?bigint AND \"used\" = ?boolean AND \"expires_at\" > ?text",
        &[Sql::Text(ts(now)), Sql::Int(user_id as i64), Sql::Bool(false), Sql::Text(ts(now))],
    )?;
    let id = random_hex(16);
    let code = new_code();
    conn.exec(
        "INSERT INTO \"_quadra_login_codes\" (\"id\", \"user_id\", \"code_hash\", \"attempts\", \"created_at\", \"expires_at\", \"used\") VALUES (?text, ?bigint, ?text, ?bigint, ?text, ?text, ?boolean)",
        &[
            Sql::Text(id.clone()),
            Sql::Int(user_id as i64),
            Sql::Text(code_hash(&id, &code)),
            Sql::Int(0),
            Sql::Text(ts(now)),
            Sql::Text(ts(now + chrono::Duration::minutes(CODE_MINUTES))),
            Sql::Bool(false),
        ],
    )?;
    Ok(Ok((id, code)))
}

/// A code typed for a request: Ok(user) once, or Err(why): `unknown`, `used`, `locked`,
/// `expired`, `wrong` (with the attempts left).
pub fn code_verify(conn: &mut Conn, id: &str, code: &str, now: NaiveDateTime) -> Result<Result<u64, (&'static str, i64)>, String> {
    let rows = conn.query(
        "SELECT CAST(\"user_id\" AS TEXT), \"code_hash\", CAST(\"attempts\" AS TEXT), \"expires_at\", CAST(\"used\" AS TEXT) FROM \"_quadra_login_codes\" WHERE \"id\" = ?text",
        &[Sql::Text(id.into())],
    )?;
    let Some(r) = rows.first() else {
        return Ok(Err(("unknown", 0)));
    };
    let truthy = |s: &Option<String>| matches!(s.as_deref(), Some("1") | Some("true") | Some("t"));
    let attempts: i64 = r[2].as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
    if truthy(&r[4]) {
        return Ok(Err(("used", 0)));
    }
    if attempts >= CODE_ATTEMPTS {
        return Ok(Err(("locked", 0)));
    }
    if r[3].as_deref().unwrap_or("") <= ts(now).as_str() {
        return Ok(Err(("expired", 0)));
    }
    let want = r[1].clone().unwrap_or_default();
    let got = code_hash(id, code);
    let same = want.len() == got.len() && want.bytes().zip(got.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
    if !same {
        conn.exec("UPDATE \"_quadra_login_codes\" SET \"attempts\" = \"attempts\" + 1 WHERE \"id\" = ?text", &[Sql::Text(id.into())])?;
        let left = CODE_ATTEMPTS - attempts - 1;
        return Ok(Err(if left > 0 { ("wrong", left) } else { ("locked", 0) }));
    }
    conn.exec("UPDATE \"_quadra_login_codes\" SET \"used\" = ?boolean WHERE \"id\" = ?text", &[Sql::Bool(true), Sql::Text(id.into())])?;
    Ok(Ok(r[0].as_deref().and_then(|v| v.parse().ok()).unwrap_or(0)))
}

#[cfg(test)]
mod code_tests {
    use super::*;

    #[test]
    fn codes_are_crockford_and_typed_loosely() {
        for _ in 0..100 {
            let c = new_code();
            assert_eq!(c.len(), 9);
            assert!(normalize_code(&c).bytes().all(|b| ALPHABET.contains(&b)));
        }
        assert_eq!(normalize_code("o1il-abcd"), "0111ABCD");
    }
}
