//! The log of a tenant outside its database: each batch the sequencer writes becomes a
//! *segment* (the statements of the batch, its row of `_quadra_batches` included), and from time
//! to time the whole database becomes a *snapshot*. A snapshot and the segments after it, applied
//! in order, give back the database as of any batch, or any transaction: that is how a tenant
//! wakes up, moves to another machine or goes back in time.
//!
//! A place in the log is a *position*: the number of the last batch and the last transaction.
//! Batches are numbered one after the other; most commit transactions, and some only change the
//! tables outside the hash chain (sign-in, saved views: `Sequencer::side`), with no transaction
//! of their own. So the log is ordered by batch, and the transaction says where data is.
//!
//! A segment is JSON: `{"first", "number", "first_tx", "last_tx", "hash", "statements": [[sql,
//! [param…]]…]}` (its first and last batch, its transactions) with each parameter `null`,
//! `{"i": n}`, `{"t": "…"}` or `{"b": true}`. Segments are applied to the dialect they were
//! written for (SQLite for SQLite).
//!
//! Names sort in the order of the log: a segment `<first>-<last>-<last_tx>.json`, a snapshot
//! `<batch>-<tx>.snapshot.db` (numbers of 20 digits).

use crate::conn::{Conn, Dialect, Sql};
use crate::sequencer::{Batch, DurabilitySink};
use serde_json::{json, Value as J};
use std::io::Write;
use std::path::{Path, PathBuf};

fn param(p: &Sql) -> J {
    match p {
        Sql::Null => J::Null,
        Sql::Int(i) => json!({ "i": i }),
        Sql::Text(t) => json!({ "t": t }),
        Sql::Bool(b) => json!({ "b": b }),
    }
}

fn unparam(v: &J) -> Result<Sql, String> {
    if v.is_null() {
        return Ok(Sql::Null);
    }
    if let Some(i) = v.get("i").and_then(J::as_i64) {
        return Ok(Sql::Int(i));
    }
    if let Some(t) = v.get("t").and_then(J::as_str) {
        return Ok(Sql::Text(t.to_string()));
    }
    if let Some(b) = v.get("b").and_then(J::as_bool) {
        return Ok(Sql::Bool(b));
    }
    Err(format!("not a parameter: {v}"))
}

/// A segment read back.
#[derive(Debug, Clone)]
pub struct Segment {
    /// its first batch
    pub first: u64,
    /// its last batch
    pub number: u64,
    pub first_tx: u64,
    pub last_tx: u64,
    pub hash: String,
    pub statements: Vec<(String, Vec<Sql>)>,
}

/// The bytes of a batch's segment.
pub fn encode(b: &Batch) -> Vec<u8> {
    let statements: Vec<J> = b
        .statements
        .iter()
        .map(|(sql, ps)| json!([sql, ps.iter().map(param).collect::<Vec<_>>()]))
        .collect();
    json!({ "first": b.first_number, "number": b.number, "first_tx": b.first_tx, "last_tx": b.last_tx, "hash": b.hash, "statements": statements }).to_string().into_bytes()
}

pub fn decode(bytes: &[u8]) -> Result<Segment, String> {
    let v: J = serde_json::from_slice(bytes).map_err(|e| format!("not a segment: {e}"))?;
    let n = |k: &str| {
        v[k].as_u64()
            .ok_or_else(|| format!("not a segment: no {k}"))
    };
    let mut statements = Vec::new();
    for s in v["statements"]
        .as_array()
        .ok_or("not a segment: no statements")?
    {
        let sql = s[0]
            .as_str()
            .ok_or("not a segment: a statement without SQL")?
            .to_string();
        let ps = s[1]
            .as_array()
            .ok_or("not a segment: a statement without parameters")?
            .iter()
            .map(unparam)
            .collect::<Result<Vec<_>, _>>()?;
        statements.push((sql, ps));
    }
    let number = n("number")?;
    Ok(Segment {
        first: v["first"].as_u64().unwrap_or(number),
        number,
        first_tx: n("first_tx")?,
        last_tx: n("last_tx")?,
        hash: v["hash"].as_str().unwrap_or_default().to_string(),
        statements,
    })
}

/// The name of a segment's file or object.
pub fn name(first: u64, last: u64, last_tx: u64) -> String {
    format!("{first:020}-{last:020}-{last_tx:020}.json")
}

/// The batches a segment name covers, and its last transaction: (first, last, last_tx).
pub fn parse_name(name: &str) -> Option<(u64, u64, u64)> {
    let mut it = name.strip_suffix(".json")?.split('-');
    let r = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(r)
}

/// The name of a snapshot at a position, before its suffix (`.snapshot.db`, `.snapshot.db.gz`).
pub fn snapshot_stem(batch: u64, tx: u64) -> String {
    format!("{batch:020}-{tx:020}")
}

/// The position of a snapshot from its name: (batch, tx).
pub fn parse_snapshot(name: &str, suffix: &str) -> Option<(u64, u64)> {
    let (b, t) = name.strip_suffix(suffix)?.split_once('-')?;
    Some((b.parse().ok()?, t.parse().ok()?))
}

/// The last transaction a database holds.
pub fn head(conn: &mut Conn) -> Result<u64, String> {
    crate::source::head(conn)
}

/// Where a database stands in its log: its last batch and its last transaction.
pub fn position(conn: &mut Conn) -> Result<(u64, u64), String> {
    let batch = if conn.table_exists("_quadra_batches")? {
        conn.query(
            "SELECT CAST(MAX(\"number\") AS TEXT) FROM \"_quadra_batches\"",
            &[],
        )?
        .first()
        .and_then(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
    } else {
        0
    };
    Ok((batch, head(conn)?))
}

/// The batches of a segment (one segment may hold several, kept together): each ends with its
/// row of `_quadra_batches`, which says its number and its first and last transaction (a batch
/// with no transaction of its own has `first_tx = last_tx + 1`): (number, first_tx, last_tx,
/// statements).
fn batches(s: &Segment) -> Vec<(u64, u64, u64, &[(String, Vec<Sql>)])> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, (sql, ps)) in s.statements.iter().enumerate() {
        if sql.starts_with("INSERT INTO \"_quadra_batches\"") {
            let n = |k: usize| match ps.get(k) {
                Some(Sql::Int(v)) => *v as u64,
                _ => 0,
            };
            out.push((n(0), n(1), n(2), &s.statements[start..=i]));
            start = i + 1;
        }
    }
    if start < s.statements.len() {
        // (a segment written without batch rows: one batch)
        out.push((s.number, s.first_tx, s.last_tx, &s.statements[start..]));
    }
    out
}

/// Applies segments, in order, to a database: batch by batch, each one transaction. A batch the
/// database already holds is skipped; a gap is an error.
pub fn apply(conn: &mut Conn, segments: &[Segment]) -> Result<u64, String> {
    apply_until(conn, segments, None)
}

/// The statements of a batch up to the commit row of `tx` (each transaction of a batch ends with
/// its row of `_quadra_commits`), then the batch's state row (its counters are past every id the
/// transactions before took): the database as of `tx`. Its batch row is left out: the copy is a
/// fork, the batches after it do not apply to it.
fn cut(statements: &[(String, Vec<Sql>)], tx: u64) -> Option<(Vec<(String, Vec<Sql>)>, u64)> {
    // the last transaction of the batch at or before `tx` (a number given up has no commit row)
    let (end, at) = statements
        .iter()
        .enumerate()
        .filter_map(|(i, (sql, ps))| match ps.first() {
            Some(Sql::Int(t))
                if sql.starts_with("INSERT INTO \"_quadra_commits\"") && *t as u64 <= tx =>
            {
                Some((i, *t as u64))
            }
            _ => None,
        })
        .last()?;
    let mut out = statements[..=end].to_vec();
    // the state row says where the database is: at the cut
    for (sql, ps) in statements.iter().filter(|(sql, _)| {
        sql.starts_with("INSERT INTO \"_quadra_meta\"") && sql.contains("'state'")
    }) {
        let ps = ps
            .iter()
            .map(|p| match p {
                Sql::Text(t) => match serde_json::from_str::<J>(t) {
                    Ok(mut v) if v.get("tx").is_some() => {
                        v["tx"] = json!(at);
                        Sql::Text(v.to_string())
                    }
                    _ => p.clone(),
                },
                _ => p.clone(),
            })
            .collect();
        out.push((sql.clone(), ps));
    }
    Some((out, at))
}

/// `apply`, stopping at transaction `until` when given (a point in time inside the log): the
/// batches without a transaction of their own before the next one are applied too. Returns the
/// last transaction of the database.
pub fn apply_until(
    conn: &mut Conn,
    segments: &[Segment],
    until: Option<u64>,
) -> Result<u64, String> {
    let (mut batch, mut at) = position(conn)?;
    if until.is_some_and(|u| u < at) {
        return Err(format!(
            "the database is already past transaction {}: at {at}",
            until.unwrap()
        ));
    }
    for s in segments {
        for (number, first, last, statements) in batches(s) {
            if number <= batch {
                continue;
            }
            if until.is_some_and(|u| at >= u && last > u) {
                return Ok(at);
            }
            if number != batch + 1 || first != at + 1 {
                return Err(format!("the log has a gap: the database is at batch {batch} (transaction {at}), the next is batch {number} (transactions {first}–{last})"));
            }
            let partial;
            let (statements, last) = match until {
                Some(u) if u < last => match cut(statements, u) {
                    Some((p, at)) => {
                        partial = p;
                        (&partial[..], at)
                    }
                    // no transaction of the batch at or before it: the database is as of `u` already
                    None => return Ok(at),
                },
                _ => (statements, last),
            };
            let begin = if conn.dialect() == Dialect::Sqlite {
                "BEGIN IMMEDIATE"
            } else {
                "BEGIN"
            };
            conn.batch(begin)?;
            let r = (|| -> Result<(), String> {
                for (sql, ps) in statements {
                    conn.exec(sql, ps)?;
                }
                Ok(())
            })();
            match r {
                Ok(()) => conn.batch("COMMIT")?,
                Err(e) => {
                    let _ = conn.batch("ROLLBACK");
                    return Err(format!("batch {number} ({first}–{last}): {e}"));
                }
            }
            batch = number;
            at = last;
        }
    }
    Ok(at)
}

/// A copy of a SQLite database as it is now, compact and consistent (`VACUUM INTO`), at
/// `path`; returns the position it holds. Other connections may read and write meanwhile.
pub fn snapshot(url: &str, path: &Path) -> Result<(u64, u64), String> {
    let mut conn = Conn::open(url)?;
    if conn.dialect() != Dialect::Sqlite {
        return Err(
            "snapshots are taken of SQLite databases (PostgreSQL has its own backups)".into(),
        );
    }
    let _ = std::fs::remove_file(path);
    // the copy is of one moment; what it holds is read from the copy itself
    conn.exec(
        "VACUUM INTO ?text",
        &[Sql::Text(path.display().to_string())],
    )?;
    position(&mut Conn::open(&format!("sqlite:{}", path.display()))?)
}

/// A database rebuilt at `target` from a snapshot and the segments after it.
pub fn restore(snapshot: &Path, segments: &[Segment], target: &Path) -> Result<u64, String> {
    restore_until(snapshot, segments, target, None)
}

/// `restore`, as of transaction `until` when given.
pub fn restore_until(
    snapshot: &Path,
    segments: &[Segment],
    target: &Path,
    until: Option<u64>,
) -> Result<u64, String> {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", target.display()));
    }
    std::fs::copy(snapshot, target).map_err(|e| format!("cannot copy the snapshot: {e}"))?;
    let mut conn = Conn::open(&format!("sqlite:{}", target.display()))?;
    apply_until(&mut conn, segments, until)
}

/// A destination that keeps each segment as a file in a folder, written and synced before the
/// batch is confirmed: another disk, a mounted volume.
pub struct Folder {
    pub dir: PathBuf,
}

const SNAPSHOT: &str = ".snapshot.db";

impl Folder {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Folder, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Folder { dir })
    }

    /// The snapshots in the folder: (batch, tx, file).
    fn snapshots(&self) -> Result<Vec<(u64, u64, PathBuf)>, String> {
        Ok(std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|e| {
                parse_snapshot(&e.file_name().to_string_lossy(), SNAPSHOT)
                    .map(|(b, t)| (b, t, e.path()))
            })
            .collect())
    }

    /// The latest snapshot in the folder: (batch, tx, file).
    pub fn latest_snapshot(&self) -> Result<Option<(u64, u64, PathBuf)>, String> {
        Ok(self.snapshots()?.into_iter().max_by_key(|(b, _, _)| *b))
    }

    /// The latest snapshot of a transaction at or before `tx`.
    pub fn snapshot_at_or_before(&self, tx: u64) -> Result<Option<(u64, u64, PathBuf)>, String> {
        Ok(self
            .snapshots()?
            .into_iter()
            .filter(|(_, t, _)| *t <= tx)
            .max_by_key(|(b, _, _)| *b))
    }

    /// The database rebuilt at `target` from the latest snapshot and the segments after it.
    pub fn restore(&self, target: &Path) -> Result<u64, String> {
        self.restore_to(target, None)
    }

    /// The database as of transaction `tx` (the latest when None), rebuilt at `target` from the
    /// latest snapshot before it and the segments after that. Older than the oldest snapshot the
    /// folder keeps, it cannot go (the database's own history goes further back).
    pub fn restore_to(&self, target: &Path, tx: Option<u64>) -> Result<u64, String> {
        let (batch, _, snap) = self
            .snapshot_at_or_before(tx.unwrap_or(u64::MAX))?
            .ok_or("no snapshot in the folder at or before that transaction")?;
        let segments: Vec<Segment> = self
            .segments()?
            .into_iter()
            .filter(|s| s.number > batch)
            .collect();
        restore_until(&snap, &segments, target, tx)
    }

    /// The segments in the folder, in order.
    pub fn segments(&self) -> Result<Vec<Segment>, String> {
        let mut names: Vec<(u64, u64, PathBuf)> = std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                parse_name(&e.file_name().to_string_lossy()).map(|(a, b, _)| (a, b, e.path()))
            })
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|(_, _, p)| {
                std::fs::read(&p)
                    .map_err(|e| e.to_string())
                    .and_then(|b| decode(&b))
            })
            .collect()
    }
}

impl DurabilitySink for Folder {
    fn last(&mut self) -> Result<Option<(u64, u64)>, String> {
        let mut last = self.latest_snapshot()?.map(|(b, t, _)| (b, t));
        for e in std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            if let Some((_, b, t)) = parse_name(&e.file_name().to_string_lossy()) {
                last = Some(last.map_or((b, t), |l| l.max((b, t))));
            }
        }
        Ok(last)
    }

    fn segments_after(&mut self, batch: u64) -> Result<Vec<Segment>, String> {
        Ok(self
            .segments()?
            .into_iter()
            .filter(|s| s.number > batch)
            .collect())
    }

    fn snapshot(&mut self, batch: u64, tx: u64, file: &Path) -> Result<(), String> {
        let stem = snapshot_stem(batch, tx);
        let target = self.dir.join(format!("{stem}{SNAPSHOT}"));
        let tmp = self.dir.join(format!(".{stem}.snapshot.tmp"));
        std::fs::copy(file, &tmp).map_err(|e| e.to_string())?;
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    fn prune(&mut self, batch: u64) -> Result<(), String> {
        for e in std::fs::read_dir(&self.dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let name = e.file_name().to_string_lossy().to_string();
            let old_segment = parse_name(&name).is_some_and(|(_, b, _)| b <= batch);
            let old_snapshot = parse_snapshot(&name, SNAPSHOT).is_some_and(|(b, _)| b < batch);
            if old_segment || old_snapshot {
                std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn persist(&mut self, batch: &Batch) -> Result<(), String> {
        let n = name(batch.first_number, batch.number, batch.last_tx);
        let final_path = self.dir.join(&n);
        let tmp = self.dir.join(format!(".{n}.tmp"));
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&encode(batch)).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &final_path).map_err(|e| e.to_string())?;
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}
