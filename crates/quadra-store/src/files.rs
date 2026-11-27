//! The files of `File` fields: what each is (its name, type and size), by its SHA-256. A
//! record's field holds `sha256:<hex>`; the bytes are in the store of files (`quadra-blobs`),
//! once, however many records name them: never in the database nor in its log.
//!
//! An upload is a row of `_quadra_uploads` first: for one user, one field, until it expires.
//! The browser writes the bytes to the store with a signed URL; a transaction names it with
//! `upload:<id>`, and the server checks the bytes (`hash` once checked) and keeps the file
//! (`used` once a commit names it). Uploads go after a day.

use crate::conn::{Conn, Sql};

/// A file kept: its name and type as it is downloaded, its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub hash: String,
    pub name: String,
    pub ctype: String,
    pub size: u64,
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

/// The upload a value names (`upload:<id>`), if it names one.
pub fn upload_of(value: &str) -> Option<&str> {
    let id = value.strip_prefix("upload:")?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(id)
}

/// Keep what a file is (once: the same bytes again are already there).
pub fn put(conn: &mut Conn, f: &File) -> Result<(), String> {
    if get(conn, &f.hash)?.is_some() {
        return Ok(());
    }
    conn.exec(
        "INSERT INTO \"_quadra_files\" (\"hash\", \"name\", \"type\", \"size\", \"created_at\") VALUES (?text, ?text, ?text, ?bigint, ?text)",
        &[
            Sql::Text(f.hash.clone()),
            Sql::Text(f.name.clone()),
            Sql::Text(f.ctype.clone()),
            Sql::Int(f.size as i64),
            Sql::Text(f.created_at.clone()),
        ],
    )?;
    Ok(())
}

pub fn get(conn: &mut Conn, hash: &str) -> Result<Option<File>, String> {
    let rows = conn.query(
        "SELECT \"hash\", \"name\", \"type\", CAST(\"size\" AS TEXT), \"created_at\" FROM \"_quadra_files\" WHERE \"hash\" = ?text",
        &[Sql::Text(hash.to_string())],
    )?;
    Ok(rows.into_iter().next().map(|r| File {
        hash: r[0].clone().unwrap_or_default(),
        name: r[1].clone().unwrap_or_default(),
        ctype: r[2].clone().unwrap_or_default(),
        size: r[3].as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        created_at: r[4].clone().unwrap_or_default(),
    }))
}

/// An upload given a signed URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub id: String,
    pub entity: String,
    pub field: String,
    pub user_id: u64,
    pub name: String,
    pub size: u64,
    pub created_at: String,
    pub expires_at: String,
    pub used: bool,
    /// the SHA-256 of its bytes, once the server has read and checked them
    pub hash: Option<String>,
}

pub fn add_upload(conn: &mut Conn, u: &Upload) -> Result<(), String> {
    conn.exec(
        "INSERT INTO \"_quadra_uploads\" (\"id\", \"entity\", \"field\", \"user_id\", \"name\", \"size\", \"created_at\", \"expires_at\", \"used\", \"hash\") VALUES (?text, ?text, ?text, ?bigint, ?text, ?bigint, ?text, ?text, ?boolean, ?text)",
        &[
            Sql::Text(u.id.clone()),
            Sql::Text(u.entity.clone()),
            Sql::Text(u.field.clone()),
            Sql::Int(u.user_id as i64),
            Sql::Text(u.name.clone()),
            Sql::Int(u.size as i64),
            Sql::Text(u.created_at.clone()),
            Sql::Text(u.expires_at.clone()),
            Sql::Bool(u.used),
            match &u.hash {
                Some(h) => Sql::Text(h.clone()),
                None => Sql::Null,
            },
        ],
    )?;
    Ok(())
}

pub fn upload(conn: &mut Conn, id: &str) -> Result<Option<Upload>, String> {
    let rows = conn.query(
        "SELECT \"id\", \"entity\", \"field\", CAST(\"user_id\" AS TEXT), \"name\", CAST(\"size\" AS TEXT), \"created_at\", \"expires_at\", CASE WHEN \"used\" THEN 'y' ELSE 'n' END, \"hash\" FROM \"_quadra_uploads\" WHERE \"id\" = ?text",
        &[Sql::Text(id.to_string())],
    )?;
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(rows.into_iter().next().map(|r| Upload {
        id: r[0].clone().unwrap_or_default(),
        entity: r[1].clone().unwrap_or_default(),
        field: r[2].clone().unwrap_or_default(),
        user_id: num(&r[3]),
        name: r[4].clone().unwrap_or_default(),
        size: num(&r[5]),
        created_at: r[6].clone().unwrap_or_default(),
        expires_at: r[7].clone().unwrap_or_default(),
        used: r[8].as_deref() == Some("y"),
        hash: r[9].clone(),
    }))
}

/// The upload's bytes were read and checked: they are the file of this hash.
pub fn upload_checked(conn: &mut Conn, id: &str, hash: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_uploads\" SET \"hash\" = ?text WHERE \"id\" = ?text",
        &[Sql::Text(hash.to_string()), Sql::Text(id.to_string())],
    )?;
    Ok(())
}

/// The uploads a commit named: not taken again.
pub fn uploads_used(conn: &mut Conn, ids: &[String]) -> Result<(), String> {
    for id in ids {
        conn.exec(
            "UPDATE \"_quadra_uploads\" SET \"used\" = ?boolean WHERE \"id\" = ?text",
            &[Sql::Bool(true), Sql::Text(id.clone())],
        )?;
    }
    Ok(())
}

/// The uploads made before `before` (used or not), oldest first: at most `limit`.
pub fn old_uploads(conn: &mut Conn, before: &str, limit: usize) -> Result<Vec<Upload>, String> {
    let rows = conn.query(
        &format!("SELECT \"id\" FROM \"_quadra_uploads\" WHERE \"created_at\" < ?text ORDER BY \"created_at\" LIMIT {limit}"),
        &[Sql::Text(before.to_string())],
    )?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(u) = upload(conn, r[0].as_deref().unwrap_or(""))? {
            out.push(u);
        }
    }
    Ok(out)
}

pub fn remove_uploads(conn: &mut Conn, ids: &[String]) -> Result<(), String> {
    for id in ids {
        conn.exec(
            "DELETE FROM \"_quadra_uploads\" WHERE \"id\" = ?text",
            &[Sql::Text(id.clone())],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_and_uploads_in_values() {
        let h = "a".repeat(64);
        assert_eq!(hash_of(&reference(&h)), Some(h.as_str()));
        assert_eq!(hash_of("sha256:abc"), None);
        assert_eq!(upload_of("upload:0f3a"), Some("0f3a"));
        assert_eq!(upload_of("upload:"), None);
        assert_eq!(upload_of("upload:../x"), None);
        assert_eq!(upload_of("sha256:x"), None);
    }
}
