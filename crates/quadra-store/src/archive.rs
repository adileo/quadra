//! Archives of the history: a database that grows for years keeps in its tables the values in
//! force and the history of the last years; the values replaced before a date move to an
//! archive file kept in the project's store of files (its folder, or its bucket).
//!
//! `archive` writes the file (compressed JSON lines: each row of `_quadra_history` moved, with
//! its salt or digest), removes those rows, and commits a *checkpoint*: `["x", before_tx,
//! archive, sha256, rows]` in its hash, signed like any commit. The SHA-256 is of the rows with
//! the digests of their values (`identity`): a value erased later leaves the archive too
//! (`redact`), and the archive is still the checkpoint's. The commits up to `before_tx`
//! can no longer be computed from the database alone: `quadra db verify` checks their links
//! and signatures, and `quadra db verify --full` reads the archives back (their SHA-256 must
//! be the checkpoint's) and computes every commit.
//!
//! What a frozen record may read as it was (`Model::read_as_of`) is never archived, nor the
//! value in force of any field.

use crate::conn::Sql;
use crate::db::{commit_hash, Database};
use chrono::NaiveDateTime;
use quadra_ir::Model;
use quadra_types::Report;
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// A checkpoint: the transaction that committed it, the last transaction archived, the archive
/// (its key in the store of files), its SHA-256, the rows in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    pub tx: u64,
    pub before_tx: u64,
    pub archive: String,
    pub sha256: String,
    pub rows: u64,
}

/// A row of the history read back from an archive: (seq, tx, record, field, value, salt,
/// digest).
pub type Row = (
    i64,
    i64,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// A row of an archive as kept: the row, and the entity of its record.
type Kept = (Row, String);

/// What identifies an archive in its checkpoint: the SHA-256 of its rows with the digests of
/// their values (`seq|tx|record|field|digest`, a line each), not of the file. A value erased
/// later is taken out of the archive (its digest kept), and the archive is still the one its
/// checkpoint names.
fn identity(rows: &[Kept]) -> String {
    let mut h = Sha256::new();
    for ((seq, tx, rec, f, v, salt, d), _) in rows {
        let d = match d {
            Some(d) => d.clone(),
            None => crate::db::value_digest(
                salt.as_deref().unwrap_or(""),
                v.as_deref().unwrap_or("null"),
            ),
        };
        h.update(format!("{seq}|{tx}|{rec}|{f}|{d}\n").as_bytes());
    }
    hex(&h.finalize())
}

fn encode(rows: &[Kept]) -> Result<Vec<u8>, String> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for ((seq, tx, rec, f, v, salt, d), ent) in rows {
        let line = json!([seq, tx, rec, f, v, salt, d, ent]);
        gz.write_all(line.to_string().as_bytes())
            .and_then(|_| gz.write_all(b"\n"))
            .map_err(|e| e.to_string())?;
    }
    gz.finish().map_err(|e| e.to_string())
}

fn decode(name: &str, bytes: &[u8]) -> Result<Vec<Kept>, String> {
    let mut text = String::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_string(&mut text)
        .map_err(|e| format!("{name}: {e}"))?;
    let mut out = Vec::new();
    for l in text.lines().filter(|l| !l.is_empty()) {
        let v: J = serde_json::from_str(l).map_err(|e| format!("{name}: {e}"))?;
        let s = |i: usize| v[i].as_str().map(str::to_string);
        out.push((
            (
                v[0].as_i64().unwrap_or(0),
                v[1].as_i64().unwrap_or(0),
                v[2].as_i64().unwrap_or(0),
                s(3).unwrap_or_default(),
                s(4),
                s(5),
                s(6),
            ),
            s(7).unwrap_or_default(),
        ));
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn num(s: &Option<String>) -> i64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

/// The checkpoints of a database, in order.
pub fn checkpoints(conn: &mut crate::Conn) -> Result<Vec<Checkpoint>, String> {
    Ok(conn
        .query("SELECT CAST(\"tx\" AS TEXT), CAST(\"before_tx\" AS TEXT), \"archive\", \"sha256\", CAST(\"rows\" AS TEXT) FROM \"_quadra_checkpoints\" ORDER BY \"_quadra_checkpoints\".\"tx\"", &[])?
        .into_iter()
        .map(|r| Checkpoint {
            tx: num(&r[0]) as u64,
            before_tx: num(&r[1]) as u64,
            archive: r[2].clone().unwrap_or_default(),
            sha256: r[3].clone().unwrap_or_default(),
            rows: num(&r[4]) as u64,
        })
        .collect())
}

/// The entry of a checkpoint in its commit's hash.
pub(crate) fn entry(c: &Checkpoint) -> J {
    json!(["x", c.before_tx, c.archive, c.sha256, c.rows])
}

/// The rows of an archive, checked against its checkpoint.
pub fn read(c: &Checkpoint, bytes: &[u8]) -> Result<Vec<Row>, String> {
    let rows = decode(&c.archive, bytes)?;
    let id = identity(&rows);
    if id != c.sha256 {
        return Err(format!(
            "{}: its rows are not those of the checkpoint (commit {}): the archive was altered",
            c.archive, c.tx
        ));
    }
    Ok(rows.into_iter().map(|(r, _)| r).collect())
}

/// Takes out of the archives the values erased after them (`erase`, a retention that ended,
/// `history`): value and salt removed, the digest kept, each archive written back where it is
/// when it changed. Returns how many values.
pub fn redact(
    conn: &mut crate::Conn,
    get: &mut dyn FnMut(&str) -> Result<Option<Vec<u8>>, String>,
    put: &mut dyn FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<usize, String> {
    let mut n = 0;
    for c in checkpoints(conn)? {
        let erasures = conn.query(
            "SELECT CAST(\"record_id\" AS TEXT), \"entity\", \"fields\", CAST(\"whole\" AS TEXT) FROM \"_quadra_erasures\" WHERE \"tx\" > ?bigint",
            &[Sql::Int(c.tx as i64)],
        )?;
        if erasures.is_empty() {
            continue;
        }
        let Some(bytes) = get(&c.archive)? else {
            continue;
        };
        let mut rows = decode(&c.archive, &bytes)?;
        let mut changed = 0;
        for ((_, _, rec, f, v, salt, d), ent) in rows.iter_mut() {
            if v.is_none() {
                continue;
            }
            let hit = erasures.iter().any(|e| {
                let id = num(&e[0]);
                let whole = matches!(e[3].as_deref(), Some("1" | "true" | "t"));
                let fields: Vec<String> =
                    serde_json::from_str(e[2].as_deref().unwrap_or("[]")).unwrap_or_default();
                // `history …` of an entity (record 0): every value of the archive is one
                // replaced before it
                (id == 0 && e[1].as_deref() == Some(ent.as_str()))
                    || (id == *rec && (whole || fields.contains(f)))
            });
            if hit {
                *d = Some(crate::db::value_digest(
                    salt.as_deref().unwrap_or(""),
                    v.as_deref().unwrap_or("null"),
                ));
                *v = None;
                *salt = None;
                changed += 1;
            }
        }
        if changed > 0 {
            put(&c.archive, &encode(&rows)?)?;
            n += changed;
        }
    }
    Ok(n)
}

/// Moves to an archive the values of the history replaced by a write committed before
/// `before`, except what a frozen record may read. `put(key, bytes)` keeps the archive (the
/// project's store of files). Run with the project stopped, as migrations are. None: nothing to
/// archive.
pub fn archive(
    url: &str,
    model: &Model,
    report: &Report,
    before: NaiveDateTime,
    time: NaiveDateTime,
    put: &mut dyn FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<Option<Checkpoint>, String> {
    let mut db = Database::open(url, model, report)?;
    let at = before.format("%Y-%m-%dT%H:%M:%S").to_string();
    let cut = db.conn.query(
        "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\" WHERE \"at\" < ?text",
        &[Sql::Text(at)],
    )?;
    let cut = num(&cut.first().and_then(|r| r[0].clone()));
    if cut == 0 {
        return Ok(None);
    }
    let whole: Vec<String> = model.read_as_of().into_iter().collect();
    let not_kept = if whole.is_empty() {
        String::new()
    } else {
        format!(
            " AND r.\"entity\" NOT IN ({})",
            whole
                .iter()
                .map(|k| format!("'{}'", k.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let rows = db.conn.query(
        &format!("SELECT CAST(h.\"seq\" AS TEXT), CAST(h.\"tx\" AS TEXT), CAST(h.\"record_id\" AS TEXT), h.\"field\", h.\"value\", h.\"salt\", h.\"digest\", r.\"entity\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE h.\"tx\" <= ?bigint{not_kept} AND EXISTS (SELECT 1 FROM \"_quadra_history\" h2 WHERE h2.\"record_id\" = h.\"record_id\" AND h2.\"field\" = h.\"field\" AND h2.\"seq\" > h.\"seq\" AND h2.\"tx\" <= ?bigint) ORDER BY h.\"seq\""),
        &[Sql::Int(cut), Sql::Int(cut)],
    )?;
    if rows.is_empty() {
        return Ok(None);
    }
    let kept: Vec<Kept> = rows
        .iter()
        .map(|r| {
            (
                (
                    num(&r[0]),
                    num(&r[1]),
                    num(&r[2]),
                    r[3].clone().unwrap_or_default(),
                    r[4].clone(),
                    r[5].clone(),
                    r[6].clone(),
                ),
                r[7].clone().unwrap_or_default(),
            )
        })
        .collect();
    let bytes = encode(&kept)?;
    let sha = identity(&kept);
    let name = format!("archive/history-{cut:020}-{}.jsonl.gz", &sha[..12]);
    put(&name, &bytes)?;
    let seqs: Vec<i64> = rows.iter().map(|r| num(&r[0])).collect();
    let mut c = Checkpoint {
        tx: 0,
        before_tx: cut as u64,
        archive: name,
        sha256: sha,
        rows: seqs.len() as u64,
    };
    let key = crate::signing::active(&mut db.conn)?;
    db.conn.batch("BEGIN")?;
    let r = (|| -> Result<u64, String> {
        for chunk in seqs.chunks(500) {
            let list: Vec<String> = chunk.iter().map(|s| s.to_string()).collect();
            db.conn.batch(&format!(
                "DELETE FROM \"_quadra_history\" WHERE \"seq\" IN ({})",
                list.join(", ")
            ))?;
        }
        let head = crate::source::head(&mut db.conn)?;
        let last = db.conn.query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"_quadra_commits\".\"tx\" DESC LIMIT 1", &[])?;
        let prev = last
            .first()
            .and_then(|r| r[0].clone())
            .unwrap_or_else(|| "0".repeat(64));
        let tx = head + 1;
        c.tx = tx;
        let at = time.format("%Y-%m-%dT%H:%M:%S").to_string();
        let cause = format!(
            "archive: {} values replaced before {}",
            c.rows,
            before.format("%Y-%m-%d")
        );
        let hash = commit_hash(&prev, tx, &at, None, &cause, &[entry(&c)]);
        let signature = crate::signing::sign_commit(&key, &hash)?;
        db.conn.exec(
            "INSERT INTO \"_quadra_checkpoints\" (\"tx\", \"before_tx\", \"archive\", \"sha256\", \"rows\") VALUES (?bigint, ?bigint, ?text, ?text, ?bigint)",
            &[Sql::Int(tx as i64), Sql::Int(cut), Sql::Text(c.archive.clone()), Sql::Text(c.sha256.clone()), Sql::Int(c.rows as i64)],
        )?;
        db.conn.exec(
            "INSERT INTO \"_quadra_commits\" (\"tx\", \"at\", \"user_id\", \"cause\", \"prev_hash\", \"hash\", \"signature\") VALUES (?bigint, ?text, ?bigint, ?text, ?text, ?text, ?text)",
            &[Sql::Int(tx as i64), Sql::Text(at), Sql::Null, Sql::Text(cause), Sql::Text(prev), Sql::Text(hash), signature],
        )?;
        let state = db.conn.query(
            "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'state'",
            &[],
        )?;
        let mut st: J = state
            .first()
            .and_then(|r| r[0].as_deref())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({ "next_id": 0 }));
        st["tx"] = json!(tx);
        db.set_meta("state", &st.to_string())?;
        Ok(tx)
    })();
    match r {
        Ok(_) => {
            db.conn.batch("COMMIT")?;
            Ok(Some(c))
        }
        Err(e) => {
            let _ = db.conn.batch("ROLLBACK");
            Err(e)
        }
    }
}
