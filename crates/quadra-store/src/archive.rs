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

/// `quadra db archive --before …` and the sweep's `[data] archive_after`: the values replaced
/// before `before` (and every value of the records taken away or deleted by then: their
/// digests) moved to an archive (`put` writes it, in the store of files), the checkpoint a
/// commit of its own, through the project's sequencer when it has one. The values in force
/// stay, and every value of the entities frozen records read (`Model::read_as_of`). None:
/// nothing to archive.
pub fn archive(
    url: &str,
    model: &Model,
    report: &Report,
    before: NaiveDateTime,
    time: NaiveDateTime,
    put: &mut dyn FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<Option<Checkpoint>, String> {
    archive_with(url, model, report, before, time, None, put)
}

/// `archive`, committed through `sequencer` when there is one.
pub fn archive_with(
    url: &str,
    model: &Model,
    report: &Report,
    before: NaiveDateTime,
    time: NaiveDateTime,
    sequencer: Option<&crate::Sequencer>,
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
            " AND COALESCE(r.\"entity\", x.\"entity\") NOT IN ({})",
            whole
                .iter()
                .map(|k| format!("'{}'", k.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    // a value replaced by then; any value of a record gone by then (its digests: erased); never
    // a value of `history none` (outside the chain)
    let rows = db.conn.query(
        &format!("SELECT CAST(h.\"seq\" AS TEXT), CAST(h.\"tx\" AS TEXT), CAST(h.\"record_id\" AS TEXT), h.\"field\", h.\"value\", h.\"salt\", h.\"digest\", COALESCE(r.\"entity\", x.\"entity\") FROM \"_quadra_history\" h LEFT JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" LEFT JOIN \"_quadra_erased\" x ON x.\"id\" = h.\"record_id\" WHERE h.\"tx\" <= ?bigint AND (h.\"salt\" IS NULL OR h.\"salt\" <> ''){not_kept} AND (EXISTS (SELECT 1 FROM \"_quadra_history\" h2 WHERE h2.\"record_id\" = h.\"record_id\" AND h2.\"field\" = h.\"field\" AND h2.\"seq\" > h.\"seq\" AND h2.\"tx\" <= ?bigint) OR (r.\"id\" IS NULL AND x.\"erased_tx\" <= ?bigint)) ORDER BY h.\"seq\""),
        &[Sql::Int(cut), Sql::Int(cut), Sql::Int(cut)],
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
    let n = seqs.len() as u64;
    drop(db);
    let cause = format!(
        "archive: {n} values replaced before {}",
        before.format("%Y-%m-%d")
    );
    let write = quadra_runtime::store::CheckpointWrite {
        before_tx: cut as u64,
        archive: name.clone(),
        sha256: sha.clone(),
        seqs,
    };
    let tx = crate::ops::with_engine(url, model, report, time, sequencer, |i| {
        i.system += 1;
        let r = i.with_cause(cause.clone(), |i| {
            i.transaction(|i| {
                i.store.checkpoint = Some(write.clone());
                Ok(())
            })
        });
        i.system -= 1;
        r.map_err(|e| format!("archive: {e:?}"))?;
        Ok(i.store.tx)
    })?;
    Ok(Some(Checkpoint {
        tx,
        before_tx: cut as u64,
        archive: name,
        sha256: sha,
        rows: n,
    }))
}

/// JSON lines, compressed (`.jsonl.gz`): the archives of the messages and events whose term
/// ended (`keep …, then archive`).
pub fn gz_lines(lines: &[serde_json::Value]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for l in lines {
        writeln!(gz, "{l}").map_err(|e| e.to_string())?;
    }
    gz.finish().map_err(|e| e.to_string())
}
