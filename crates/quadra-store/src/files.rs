//! The files the app makes and keeps: documents rendered by `attach` (an invoice's PDF), by
//! their SHA-256. A record's `File` field holds `sha256:<hex>`; the bytes are here once, however
//! many records name them, in base64 (as every value of the log is text).

use crate::conn::{Conn, Sql};

/// A file kept: its name and type as it is downloaded, its size, its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub hash: String,
    pub name: String,
    pub ctype: String,
    pub size: u64,
    pub content_b64: String,
    pub created_at: String,
}

/// What a `File` field holds for a file of this hash.
pub fn reference(hash: &str) -> String {
    format!("sha256:{hash}")
}

/// The hash a `File` field names (`sha256:<hex>`), if it names one.
pub fn hash_of(field: &str) -> Option<&str> {
    let h = field.strip_prefix("sha256:")?;
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// Keep a file (once: the same bytes again are already there).
pub fn put(conn: &mut Conn, f: &File) -> Result<(), String> {
    if get(conn, &f.hash)?.is_some() {
        return Ok(());
    }
    conn.exec(
        "INSERT INTO \"_quadra_files\" (\"hash\", \"name\", \"type\", \"size\", \"content\", \"created_at\") VALUES (?text, ?text, ?text, ?bigint, ?text, ?text)",
        &[
            Sql::Text(f.hash.clone()),
            Sql::Text(f.name.clone()),
            Sql::Text(f.ctype.clone()),
            Sql::Int(f.size as i64),
            Sql::Text(f.content_b64.clone()),
            Sql::Text(f.created_at.clone()),
        ],
    )?;
    Ok(())
}

pub fn get(conn: &mut Conn, hash: &str) -> Result<Option<File>, String> {
    let rows = conn.query(
        "SELECT \"hash\", \"name\", \"type\", CAST(\"size\" AS TEXT), \"content\", \"created_at\" FROM \"_quadra_files\" WHERE \"hash\" = ?text",
        &[Sql::Text(hash.to_string())],
    )?;
    Ok(rows.into_iter().next().map(|r| File {
        hash: r[0].clone().unwrap_or_default(),
        name: r[1].clone().unwrap_or_default(),
        ctype: r[2].clone().unwrap_or_default(),
        size: r[3].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        content_b64: r[4].clone().unwrap_or_default(),
        created_at: r[5].clone().unwrap_or_default(),
    }))
}
