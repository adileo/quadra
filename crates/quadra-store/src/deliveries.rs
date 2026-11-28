//! The deliveries of the events with an `outbound` (`outbound OrderShipped via shop`): which
//! are due, and what came of each attempt. The engine reads them on its own connection; the
//! outcomes are written through the sequencer (`Sequencer::side`), so that they reach the
//! tenant's log like any write and a node that takes the tenant over does not send again what
//! was delivered.
//!
//! An event is pending until it is delivered or fails for good (`failed_at`): a refusal that
//! will not change (an HTTP 4xx, an SMTP 5xx), or a day of failed attempts. Between attempts
//! it waits (`next_at`), longer each time.

use crate::conn::{Conn, Sql};
use serde_json::Value as J;

/// An event due for delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct Due {
    pub id: u64,
    pub tx: u64,
    pub event: String,
    pub fields: J,
    pub created_at: String,
    /// attempts made before this one
    pub attempts: u64,
    /// the hash of the commit that emitted it: with the id, what tells this event from one
    /// that reuses its number after the database was restored to an earlier point
    pub commit: String,
}

/// What came of an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// failed, tried again at `at`
    Retry {
        at: String,
        error: String,
    },
    /// failed for good
    Failed {
        error: String,
    },
}

fn in_list(events: &[String]) -> (String, Vec<Sql>) {
    let marks = vec!["?text"; events.len()].join(", ");
    (marks, events.iter().map(|e| Sql::Text(e.clone())).collect())
}

/// The pending events of these kinds whose time has come (`now`, as the outbox writes times),
/// oldest first, at most `limit`, emitted by transactions up to `durable` (the last one the
/// sequencer has confirmed where its durability rule wants it: nothing leaves for a commit that
/// could still be lost).
pub fn due(
    conn: &mut Conn,
    events: &[String],
    now: &str,
    durable: u64,
    limit: usize,
) -> Result<Vec<Due>, String> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let (marks, mut params) = in_list(events);
    params.push(Sql::Text(now.to_string()));
    params.push(Sql::Int(durable.min(i64::MAX as u64) as i64));
    let rows = conn.query(
        &format!("SELECT CAST(o.\"id\" AS TEXT), CAST(o.\"tx\" AS TEXT), o.\"event\", o.\"fields\", o.\"created_at\", CAST(o.\"attempts\" AS TEXT), c.\"hash\" FROM \"_quadra_outbox\" o LEFT JOIN \"_quadra_commits\" c ON c.\"tx\" = o.\"tx\" WHERE o.\"state\" = 'pending' AND o.\"event\" IN ({marks}) AND (o.\"next_at\" IS NULL OR o.\"next_at\" <= ?text) AND o.\"tx\" <= ?bigint AND (o.\"lane\" IS NULL OR NOT EXISTS (SELECT 1 FROM \"_quadra_outbox\" b WHERE b.\"event\" = o.\"event\" AND b.\"lane\" = o.\"lane\" AND b.\"id\" < o.\"id\" AND b.\"state\" IN ('pending', 'failed'))) ORDER BY o.\"id\" LIMIT {limit}"),
        &params,
    )?;
    let num = |v: &Option<String>| v.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);
    Ok(rows
        .into_iter()
        .map(|r| Due {
            id: num(&r[0]),
            tx: num(&r[1]),
            event: r[2].clone().unwrap_or_default(),
            fields: r[3]
                .as_deref()
                .and_then(|f| serde_json::from_str(f).ok())
                .unwrap_or(J::Null),
            created_at: r[4].clone().unwrap_or_default(),
            attempts: num(&r[5]),
            commit: r[6].clone().unwrap_or_default(),
        })
        .collect())
}

/// When the next pending event of these kinds is due: `Some("")` when one is due now (it has
/// never been tried), `None` when none is pending.
pub fn next_at(conn: &mut Conn, events: &[String]) -> Result<Option<String>, String> {
    if events.is_empty() {
        return Ok(None);
    }
    let (marks, params) = in_list(events);
    let rows = conn.query(
        &format!("SELECT COUNT(*), MIN(COALESCE(\"next_at\", '')) FROM \"_quadra_outbox\" WHERE \"state\" = 'pending' AND \"event\" IN ({marks})"),
        &params,
    )?;
    let row = rows.into_iter().next().unwrap_or_default();
    let count: u64 = row
        .first()
        .and_then(|c| c.as_deref())
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Ok((count > 0).then(|| row.get(1).cloned().flatten().unwrap_or_default()))
}

/// Pending events of kind `event` whose keys (`ordered by`, `latest by`) are not computed yet,
/// oldest first, at most `limit`: the engine computes them before choosing what is due.
pub fn without_keys(
    conn: &mut Conn,
    event: &str,
    durable: u64,
    limit: usize,
) -> Result<Vec<Due>, String> {
    let rows = conn.query(
        &format!("SELECT CAST(o.\"id\" AS TEXT), CAST(o.\"tx\" AS TEXT), o.\"event\", o.\"fields\", o.\"created_at\", CAST(o.\"attempts\" AS TEXT), c.\"hash\" FROM \"_quadra_outbox\" o LEFT JOIN \"_quadra_commits\" c ON c.\"tx\" = o.\"tx\" WHERE o.\"state\" = 'pending' AND o.\"event\" = ?text AND o.\"lane\" IS NULL AND o.\"latest\" IS NULL AND o.\"tx\" <= ?bigint ORDER BY o.\"id\" LIMIT {limit}"),
        &[Sql::Text(event.to_string()), Sql::Int(durable.min(i64::MAX as u64) as i64)],
    )?;
    let num = |v: &Option<String>| v.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);
    Ok(rows
        .into_iter()
        .map(|r| Due {
            id: num(&r[0]),
            tx: num(&r[1]),
            event: r[2].clone().unwrap_or_default(),
            fields: r[3]
                .as_deref()
                .and_then(|f| serde_json::from_str(f).ok())
                .unwrap_or(J::Null),
            created_at: r[4].clone().unwrap_or_default(),
            attempts: num(&r[5]),
            commit: r[6].clone().unwrap_or_default(),
        })
        .collect())
}

/// Records the keys of event `id` (an empty text when its outbound has none: computed). With a
/// `latest` key, the older events still pending with the same one are superseded: only the
/// newest goes out. Returns how many were superseded.
pub fn set_keys(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    lane: &str,
    latest: &str,
) -> Result<usize, String> {
    conn.exec(
        "UPDATE \"_quadra_outbox\" SET \"lane\" = ?text, \"latest\" = ?text WHERE \"id\" = ?bigint",
        &[
            Sql::Text(lane.to_string()),
            Sql::Text(latest.to_string()),
            Sql::Int(id as i64),
        ],
    )?;
    if latest.is_empty() {
        return Ok(0);
    }
    let older: Vec<u64> = conn
        .query(
            "SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_outbox\" WHERE \"event\" = ?text AND \"latest\" = ?text AND \"id\" < ?bigint AND \"state\" = 'pending'",
            &[
                Sql::Text(name.to_string()),
                Sql::Text(latest.to_string()),
                Sql::Int(id as i64),
            ],
        )?
        .into_iter()
        .filter_map(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
        .collect();
    for o in &older {
        conn.exec(
            "UPDATE \"_quadra_outbox\" SET \"state\" = 'superseded', \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"state\" = 'pending'",
            &[Sql::Int(*o as i64)],
        )?;
        log_step(
            conn,
            *o,
            name,
            now,
            "supersede",
            "superseded",
            None,
            None,
            None,
            Some(&format!("a newer one, #{id}, has the same `latest by`")),
        )?;
    }
    Ok(older.len())
}

/// A pending event that will not be sent, by its outbound's policy: `expired` (`expires
/// after`) or `skipped` (`unless`), with why. Returns whether it changed.
pub fn finish(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    state: &str,
    reason: &str,
) -> Result<bool, String> {
    let n = conn.exec(
        "UPDATE \"_quadra_outbox\" SET \"state\" = ?text, \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"state\" = 'pending'",
        &[Sql::Text(state.to_string()), Sql::Int(id as i64)],
    )?;
    if n > 0 {
        let step = if state == "expired" { "expire" } else { "skip" };
        log_step(
            conn,
            id,
            name,
            now,
            step,
            state,
            None,
            None,
            Some("policy"),
            Some(reason),
        )?;
    }
    Ok(n > 0)
}

/// A pending event not to be tried before `at` (`delay`, `window`), without an attempt.
pub fn defer(conn: &mut Conn, id: u64, at: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_outbox\" SET \"next_at\" = ?text WHERE \"id\" = ?bigint AND \"state\" = 'pending'",
        &[Sql::Text(at.to_string()), Sql::Int(id as i64)],
    )?;
    Ok(())
}

/// What an attempt was, besides its outcome: for the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attempt {
    pub connection: String,
    /// the HTTP status, or the SMTP reply code
    pub status: Option<u16>,
    pub duration_ms: u64,
    /// the beginning of the answer (never a credential: what the receiver sent back)
    pub excerpt: Option<String>,
}

/// A row of the log of a message (`_quadra_exchanges`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub at: String,
    pub step: String,
    pub outcome: String,
    pub connection: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub excerpt: Option<String>,
    pub by: Option<String>,
    pub reason: Option<String>,
}

/// A reaction to the outcome of event `id` that failed (`on … delivered` / `failed`): in its
/// log, to be looked at and fixed with a deploy.
pub fn log_reaction(
    conn: &mut Conn,
    id: u64,
    name: &str,
    at: &str,
    error: &str,
) -> Result<(), String> {
    log_step(
        conn,
        id,
        name,
        at,
        "reaction",
        "rejected",
        None,
        Some(error),
        None,
        None,
    )
}

/// Adds a row to the log of event `id` (outgoing).
#[allow(clippy::too_many_arguments)]
fn log_step(
    conn: &mut Conn,
    id: u64,
    name: &str,
    at: &str,
    step: &str,
    outcome: &str,
    attempt: Option<&Attempt>,
    error: Option<&str>,
    by: Option<&str>,
    reason: Option<&str>,
) -> Result<(), String> {
    log_dir(
        conn, "out", id, name, at, step, outcome, attempt, error, by, reason,
    )
}

/// Adds a row to the log of a message, out or in.
#[allow(clippy::too_many_arguments)]
fn log_dir(
    conn: &mut Conn,
    direction: &str,
    id: u64,
    name: &str,
    at: &str,
    step: &str,
    outcome: &str,
    attempt: Option<&Attempt>,
    error: Option<&str>,
    by: Option<&str>,
    reason: Option<&str>,
) -> Result<(), String> {
    let next: i64 = conn
        .query(
            "SELECT CAST(COALESCE(MAX(\"id\"), 0) + 1 AS TEXT) FROM \"_quadra_exchanges\"",
            &[],
        )?
        .first()
        .and_then(|r| r[0].as_deref())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let opt = |v: Option<&str>| v.map(|s| Sql::Text(s.to_string())).unwrap_or(Sql::Null);
    conn.exec(
        "INSERT INTO \"_quadra_exchanges\" (\"id\", \"at\", \"direction\", \"message\", \"name\", \"connection\", \"step\", \"outcome\", \"status\", \"duration_ms\", \"error\", \"excerpt\", \"by\", \"reason\") VALUES (?bigint, ?text, ?text, ?bigint, ?text, ?text, ?text, ?text, ?bigint, ?bigint, ?text, ?text, ?text, ?text)",
        &[
            Sql::Int(next),
            Sql::Text(at.to_string()),
            Sql::Text(direction.to_string()),
            Sql::Int(id as i64),
            Sql::Text(name.to_string()),
            opt(attempt.map(|a| a.connection.as_str())),
            Sql::Text(step.to_string()),
            Sql::Text(outcome.to_string()),
            attempt.and_then(|a| a.status).map(|s| Sql::Int(s as i64)).unwrap_or(Sql::Null),
            attempt.map(|a| Sql::Int(a.duration_ms as i64)).unwrap_or(Sql::Null),
            opt(error),
            opt(attempt.and_then(|a| a.excerpt.as_deref())),
            opt(by),
            opt(reason),
        ],
    )?;
    Ok(())
}

/// Writes what came of an attempt on event `id` (named `name`) at `now`, and the attempt in its
/// log. An event that is no longer pending is left as it is: returns whether it changed.
pub fn record(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    outcome: &Outcome,
    attempt: &Attempt,
) -> Result<bool, String> {
    let key = Sql::Int(id as i64);
    let at = Sql::Text(now.to_string());
    let pending = "\"id\" = ?bigint AND \"state\" = 'pending'";
    let (n, state, error) = match outcome {
        Outcome::Delivered => (
            conn.exec(
                &format!("UPDATE \"_quadra_outbox\" SET \"delivered\" = ?boolean, \"delivered_at\" = ?text, \"state\" = 'delivered', \"attempts\" = \"attempts\" + 1, \"next_at\" = NULL, \"error\" = NULL WHERE {pending}"),
                &[Sql::Bool(true), at, key],
            )?,
            "delivered",
            None,
        ),
        Outcome::Retry { at: next, error } => (
            conn.exec(
                &format!("UPDATE \"_quadra_outbox\" SET \"attempts\" = \"attempts\" + 1, \"next_at\" = ?text, \"error\" = ?text WHERE {pending}"),
                &[Sql::Text(next.clone()), Sql::Text(error.clone()), key],
            )?,
            "pending",
            Some(error.as_str()),
        ),
        Outcome::Failed { error } => (
            conn.exec(
                &format!("UPDATE \"_quadra_outbox\" SET \"attempts\" = \"attempts\" + 1, \"failed_at\" = ?text, \"state\" = 'failed', \"error\" = ?text WHERE {pending}"),
                &[at, Sql::Text(error.clone()), key],
            )?,
            "failed",
            Some(error.as_str()),
        ),
    };
    if n > 0 {
        log_step(
            conn,
            id,
            name,
            now,
            "attempt",
            state,
            Some(attempt),
            error,
            None,
            None,
        )?;
    }
    Ok(n > 0)
}

/// Events that failed for good or were skipped made pending again (`quadra outbox retry`):
/// tried at the next round, logged with who asked. Returns how many.
pub fn retry(conn: &mut Conn, ids: &[u64], now: &str, by: &str) -> Result<usize, String> {
    let mut n = 0;
    for id in ids {
        let name = event_name(conn, *id)?;
        let done = conn.exec(
            "UPDATE \"_quadra_outbox\" SET \"state\" = 'pending', \"failed_at\" = NULL, \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"state\" IN ('failed', 'skipped')",
            &[Sql::Int(*id as i64)],
        )?;
        if done > 0 {
            log_step(
                conn,
                *id,
                &name.unwrap_or_default(),
                now,
                "retry",
                "pending",
                None,
                None,
                Some(by),
                None,
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Pending or failed events that will not be sent (`quadra outbox skip`), with a reason:
/// logged with who asked. Returns how many.
pub fn skip(
    conn: &mut Conn,
    ids: &[u64],
    now: &str,
    by: &str,
    reason: &str,
) -> Result<usize, String> {
    if reason.trim().is_empty() {
        return Err("a skip needs a reason (--reason \"…\")".into());
    }
    let mut n = 0;
    for id in ids {
        let name = event_name(conn, *id)?;
        let done = conn.exec(
            "UPDATE \"_quadra_outbox\" SET \"state\" = 'skipped', \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"state\" IN ('pending', 'failed')",
            &[Sql::Int(*id as i64)],
        )?;
        if done > 0 {
            log_step(
                conn,
                *id,
                &name.unwrap_or_default(),
                now,
                "skip",
                "skipped",
                None,
                None,
                Some(by),
                Some(reason),
            )?;
            n += 1;
        }
    }
    Ok(n)
}

fn event_name(conn: &mut Conn, id: u64) -> Result<Option<String>, String> {
    Ok(conn
        .query(
            "SELECT \"event\" FROM \"_quadra_outbox\" WHERE \"id\" = ?bigint",
            &[Sql::Int(id as i64)],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].clone()))
}

/// The log of event `id`, oldest first.
pub fn log(conn: &mut Conn, id: u64) -> Result<Vec<Step>, String> {
    log_of(conn, "out", id)
}

/// The log of message `id` of the inbox, oldest first.
pub fn inbox_log(conn: &mut Conn, id: u64) -> Result<Vec<Step>, String> {
    log_of(conn, "in", id)
}

fn log_of(conn: &mut Conn, direction: &str, id: u64) -> Result<Vec<Step>, String> {
    let rows = conn.query(
        "SELECT \"at\", \"step\", \"outcome\", \"connection\", CAST(\"status\" AS TEXT), CAST(\"duration_ms\" AS TEXT), \"error\", \"excerpt\", \"by\", \"reason\" FROM \"_quadra_exchanges\" WHERE \"direction\" = ?text AND \"message\" = ?bigint ORDER BY \"_quadra_exchanges\".\"id\"",
        &[Sql::Text(direction.to_string()), Sql::Int(id as i64)],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| Step {
            at: r[0].clone().unwrap_or_default(),
            step: r[1].clone().unwrap_or_default(),
            outcome: r[2].clone().unwrap_or_default(),
            connection: r[3].clone(),
            status: r[4].as_deref().and_then(|s| s.parse().ok()),
            duration_ms: r[5].as_deref().and_then(|s| s.parse().ok()),
            error: r[6].clone(),
            excerpt: r[7].clone(),
            by: r[8].clone(),
            reason: r[9].clone(),
        })
        .collect())
}

/// Takes up to `limit` pending events for an integration that delivers them itself
/// (`GET /v1/outbox`): those not of the kinds in `served` (the server delivers them), of the
/// kinds in `only` if given, not leased to another until after `now`, of commits up to
/// `durable`. Each is leased until `until`: no one else gets it before, and if it is neither
/// acknowledged nor refused by then it is handed out again.
#[allow(clippy::too_many_arguments)]
pub fn take(
    conn: &mut Conn,
    served: &[String],
    only: Option<&[String]>,
    now: &str,
    until: &str,
    durable: u64,
    limit: usize,
) -> Result<Vec<Due>, String> {
    let mut filter = String::new();
    let mut params = vec![
        Sql::Text(now.to_string()),
        Sql::Text(now.to_string()),
        Sql::Int(durable.min(i64::MAX as u64) as i64),
    ];
    if !served.is_empty() {
        let (marks, p) = in_list(served);
        filter.push_str(&format!(" AND o.\"event\" NOT IN ({marks})"));
        params.extend(p);
    }
    if let Some(only) = only {
        if only.is_empty() {
            return Ok(Vec::new());
        }
        let (marks, p) = in_list(only);
        filter.push_str(&format!(" AND o.\"event\" IN ({marks})"));
        params.extend(p);
    }
    let rows = conn.query(
        &format!("SELECT CAST(o.\"id\" AS TEXT), CAST(o.\"tx\" AS TEXT), o.\"event\", o.\"fields\", o.\"created_at\", CAST(o.\"attempts\" AS TEXT), c.\"hash\" FROM \"_quadra_outbox\" o LEFT JOIN \"_quadra_commits\" c ON c.\"tx\" = o.\"tx\" WHERE o.\"state\" = 'pending' AND (o.\"lease_until\" IS NULL OR o.\"lease_until\" <= ?text) AND (o.\"next_at\" IS NULL OR o.\"next_at\" <= ?text) AND o.\"tx\" <= ?bigint{filter} ORDER BY o.\"id\" LIMIT {limit}"),
        &params,
    )?;
    let num = |v: &Option<String>| v.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0u64);
    let mut out = Vec::new();
    for r in rows {
        let id = num(&r[0]);
        conn.exec(
            "UPDATE \"_quadra_outbox\" SET \"lease_until\" = ?text WHERE \"id\" = ?bigint",
            &[Sql::Text(until.to_string()), Sql::Int(id as i64)],
        )?;
        out.push(Due {
            id,
            tx: num(&r[1]),
            event: r[2].clone().unwrap_or_default(),
            fields: r[3]
                .as_deref()
                .and_then(|f| serde_json::from_str(f).ok())
                .unwrap_or(J::Null),
            created_at: r[4].clone().unwrap_or_default(),
            attempts: num(&r[5]),
            commit: r[6].clone().unwrap_or_default(),
        });
    }
    Ok(out)
}

/// What an integration says of an event it took: delivered (`ack`), or not, with the error,
/// tried again from `retry_at` or failed for good (`nack`). Only an event still pending and
/// not of the kinds in `served` changes; the lease ends. Returns whether it changed.
pub fn settle(
    conn: &mut Conn,
    served: &[String],
    id: u64,
    now: &str,
    outcome: &Outcome,
    by: &str,
) -> Result<bool, String> {
    let rows = conn.query(
        "SELECT \"event\" FROM \"_quadra_outbox\" WHERE \"id\" = ?bigint AND \"state\" = 'pending'",
        &[Sql::Int(id as i64)],
    )?;
    let Some(name) = rows.into_iter().next().and_then(|r| r[0].clone()) else {
        return Ok(false);
    };
    if served.contains(&name) {
        return Ok(false);
    }
    let attempt = Attempt {
        connection: format!("pull:{by}"),
        ..Default::default()
    };
    let changed = record(conn, id, &name, now, outcome, &attempt)?;
    conn.exec(
        "UPDATE \"_quadra_outbox\" SET \"lease_until\" = NULL WHERE \"id\" = ?bigint",
        &[Sql::Int(id as i64)],
    )?;
    Ok(changed)
}

/// What came of a message received by an `inbound`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Received {
    /// kept, to be handled: its id
    New(u64),
    /// already received with the same key and the same body: not kept again
    Duplicate(u64),
    /// the same key with another body: not kept, said in the log
    Conflict(u64),
}

/// A message of the inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub inbound: String,
    pub connection: String,
    pub key: String,
    pub hash: String,
    pub body: String,
    pub received_at: String,
    pub state: String,
    pub attempts: u64,
    pub error: Option<String>,
    pub done_at: Option<String>,
    pub tx: Option<u64>,
    /// tried again then (`retry for`)
    pub next_at: Option<String>,
    pub marks: Marks,
    /// the rows refused, and why (a file of rows handled `atomic per row`)
    pub report: Option<String>,
}

/// Keeps a message received by `inbound` from `connection`, raw with its hash, unless one with
/// the same key was received before (`key`: what the inbound says, else the hash). `error`: it
/// cannot be read as its record (kept all the same, `rejected`, to be handled again after a
/// deploy that reads it).
#[allow(clippy::too_many_arguments)]
/// What an inbound's lines say of a message, read when it comes: its value of `ordered by`,
/// of `latest by` and of `using`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub lane: Option<String>,
    pub latest: Option<String>,
    pub version: Option<String>,
    /// its name on the other side (a file's path, a mail's id): where it is settled once handled
    pub origin: Option<String>,
    /// the body is kept as base64 (it is not UTF-8: an EBCDIC file, an attachment)
    pub base64: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn receive(
    conn: &mut Conn,
    inbound: &str,
    connection: &str,
    key: &str,
    hash: &str,
    body: &str,
    now: &str,
    error: Option<&str>,
    marks: &Marks,
) -> Result<Received, String> {
    if let Some(r) = conn
        .query(
            "SELECT CAST(\"id\" AS TEXT), \"hash\" FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"key\" = ?text ORDER BY \"id\" LIMIT 1",
            &[Sql::Text(inbound.to_string()), Sql::Text(key.to_string())],
        )?
        .into_iter()
        .next()
    {
        let id: u64 = r[0].as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
        let same = r[1].as_deref() == Some(hash);
        let (step, why) = if same {
            ("duplicate", "the same message, received again: not handled again".to_string())
        } else {
            ("conflict", format!("another message with the key `{key}` and a different body: not kept"))
        };
        log_dir(conn, "in", id, inbound, now, step, if same { "duplicate" } else { "conflict" }, None, None, None, Some(&why))?;
        return Ok(if same { Received::Duplicate(id) } else { Received::Conflict(id) });
    }
    let id: i64 = conn
        .query(
            "SELECT CAST(COALESCE(MAX(\"id\"), 0) + 1 AS TEXT) FROM \"_quadra_inbox\"",
            &[],
        )?
        .first()
        .and_then(|r| r[0].as_deref())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let state = if error.is_some() {
        "rejected"
    } else {
        "received"
    };
    let opt = |v: &Option<String>| v.clone().map(Sql::Text).unwrap_or(Sql::Null);
    conn.exec(
        "INSERT INTO \"_quadra_inbox\" (\"id\", \"inbound\", \"connection\", \"key\", \"hash\", \"body\", \"received_at\", \"state\", \"attempts\", \"error\", \"lane\", \"latest\", \"version\", \"origin\", \"encoding\") VALUES (?bigint, ?text, ?text, ?text, ?text, ?text, ?text, ?text, ?bigint, ?text, ?text, ?text, ?text, ?text, ?text)",
        &[
            Sql::Int(id),
            Sql::Text(inbound.to_string()),
            Sql::Text(connection.to_string()),
            Sql::Text(key.to_string()),
            Sql::Text(hash.to_string()),
            Sql::Text(body.to_string()),
            Sql::Text(now.to_string()),
            Sql::Text(state.to_string()),
            Sql::Int(0),
            error.map(|e| Sql::Text(e.to_string())).unwrap_or(Sql::Null),
            opt(&marks.lane),
            opt(&marks.latest),
            opt(&marks.version),
            opt(&marks.origin),
            if marks.base64 {
                Sql::Text("base64".into())
            } else {
                Sql::Null
            },
        ],
    )?;
    log_dir(
        conn,
        "in",
        id as u64,
        inbound,
        now,
        "receive",
        state,
        Some(&Attempt {
            connection: connection.to_string(),
            ..Default::default()
        }),
        error,
        None,
        None,
    )?;
    Ok(Received::New(id as u64))
}

/// A message refused before it was kept (a wrong signature, too large, an unknown path): only
/// in the log, as message 0 of its inbound.
pub fn refuse(
    conn: &mut Conn,
    inbound: &str,
    connection: &str,
    now: &str,
    error: &str,
) -> Result<(), String> {
    log_dir(
        conn,
        "in",
        0,
        inbound,
        now,
        "refuse",
        "refused",
        Some(&Attempt {
            connection: connection.to_string(),
            ..Default::default()
        }),
        Some(error),
        None,
        None,
    )
}

fn message_of(r: &[Option<String>]) -> Message {
    let num = |v: &Option<String>| v.as_deref().and_then(|s| s.parse().ok());
    Message {
        id: num(&r[0]).unwrap_or(0),
        inbound: r[1].clone().unwrap_or_default(),
        connection: r[2].clone().unwrap_or_default(),
        key: r[3].clone().unwrap_or_default(),
        hash: r[4].clone().unwrap_or_default(),
        body: r[5].clone().unwrap_or_default(),
        received_at: r[6].clone().unwrap_or_default(),
        state: r[7].clone().unwrap_or_default(),
        attempts: num(&r[8]).unwrap_or(0),
        error: r[9].clone(),
        done_at: r[10].clone(),
        tx: num(&r[11]),
        next_at: r.get(12).cloned().flatten(),
        marks: Marks {
            lane: r.get(13).cloned().flatten(),
            latest: r.get(14).cloned().flatten(),
            version: r.get(15).cloned().flatten(),
            origin: r.get(16).cloned().flatten(),
            base64: r.get(17).cloned().flatten().as_deref() == Some("base64"),
        },
        report: r.get(18).cloned().flatten(),
    }
}

/// The messages of `inbound` from the other side (with an origin) handled for good and not
/// settled there yet: (origin, done, the error or the report of its rows).
pub fn to_settle(
    conn: &mut Conn,
    inbound: &str,
    limit: usize,
) -> Result<Vec<(String, bool, Option<String>)>, String> {
    Ok(conn
        .query(
            &format!("SELECT \"origin\", \"state\", \"error\", \"report\" FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"origin\" IS NOT NULL AND \"settled_at\" IS NULL AND \"state\" IN ('done', 'rejected', 'skipped', 'superseded') ORDER BY \"id\" LIMIT {limit}"),
            &[Sql::Text(inbound.to_string())],
        )?
        .into_iter()
        .filter_map(|r| {
            let origin = r[0].clone()?;
            let done = r[1].as_deref() != Some("rejected");
            let report = r[3].clone().or_else(|| r[2].clone());
            Some((origin, done, report))
        })
        .collect())
}

/// The messages of `inbound` from `origins` settled on the other side at `now`.
pub fn settled(
    conn: &mut Conn,
    inbound: &str,
    origins: &[String],
    now: &str,
) -> Result<(), String> {
    for o in origins {
        conn.exec(
            "UPDATE \"_quadra_inbox\" SET \"settled_at\" = ?text WHERE \"inbound\" = ?text AND \"origin\" = ?text AND \"settled_at\" IS NULL",
            &[Sql::Text(now.to_string()), Sql::Text(inbound.to_string()), Sql::Text(o.clone())],
        )?;
    }
    Ok(())
}

/// The report of the rows of message `id` (those refused, and why), kept with it.
pub fn set_report(conn: &mut Conn, id: u64, report: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_inbox\" SET \"report\" = ?text WHERE \"id\" = ?bigint",
        &[Sql::Text(report.to_string()), Sql::Int(id as i64)],
    )?;
    Ok(())
}

/// A message whose code failed, tried again at `next_at` (`retry for`): it stays `received`.
pub fn inbox_later(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    error: &str,
    next_at: &str,
) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_inbox\" SET \"attempts\" = \"attempts\" + 1, \"error\" = ?text, \"next_at\" = ?text WHERE \"id\" = ?bigint AND \"state\" = 'received'",
        &[
            Sql::Text(error.to_string()),
            Sql::Text(next_at.to_string()),
            Sql::Int(id as i64),
        ],
    )?;
    log_dir(
        conn,
        "in",
        id,
        name,
        now,
        "handle",
        "received",
        None,
        Some(error),
        None,
        Some(&format!("tried again at {next_at}")),
    )
}

/// A message older than one of the same `latest by` already handled: `superseded`, not handled.
pub fn inbox_supersede(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    by: u64,
) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_inbox\" SET \"state\" = 'superseded', \"done_at\" = ?text WHERE \"id\" = ?bigint AND \"state\" = 'received'",
        &[Sql::Text(now.to_string()), Sql::Int(id as i64)],
    )?;
    log_dir(
        conn,
        "in",
        id,
        name,
        now,
        "handle",
        "superseded",
        None,
        None,
        None,
        Some(&format!(
            "message {by}, as new or newer, was handled already"
        )),
    )
}

/// The messages of `latest` handled already, with their `using`: what a newer one is compared to.
pub fn handled_versions(
    conn: &mut Conn,
    inbound: &str,
    latest: &str,
) -> Result<Vec<(u64, String)>, String> {
    Ok(conn
        .query(
            "SELECT CAST(\"id\" AS TEXT), \"version\" FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"latest\" = ?text AND \"state\" = 'done' AND \"version\" IS NOT NULL",
            &[Sql::Text(inbound.to_string()), Sql::Text(latest.to_string())],
        )?
        .into_iter()
        .filter_map(|r| Some((r[0].as_deref()?.parse().ok()?, r[1].clone()?)))
        .collect())
}

/// The steps of the log before `before` removed (`[integrations] log_days`): the events and
/// messages themselves, with their final states, stay. Returns how many.
pub fn prune_log(conn: &mut Conn, before: &str) -> Result<usize, String> {
    Ok(conn.exec(
        "DELETE FROM \"_quadra_exchanges\" WHERE \"at\" < ?text",
        &[Sql::Text(before.to_string())],
    )? as usize)
}

/// When the next message waiting to be tried again is due (`retry for`).
pub fn inbox_next_at(conn: &mut Conn) -> Result<Option<String>, String> {
    Ok(conn
        .query(
            "SELECT MIN(\"next_at\") FROM \"_quadra_inbox\" WHERE \"state\" = 'received' AND \"next_at\" IS NOT NULL",
            &[],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].clone()))
}

/// Whether version `a` (of `using`) is as new as `b` or newer: numbers by value, the rest
/// (instants, dates, text) as text, which sorts them.
pub fn as_new(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x >= y,
        _ => a >= b,
    }
}

const MESSAGE: &str = "SELECT CAST(\"id\" AS TEXT), \"inbound\", \"connection\", \"key\", \"hash\", \"body\", \"received_at\", \"state\", CAST(\"attempts\" AS TEXT), \"error\", \"done_at\", CAST(\"tx\" AS TEXT), \"next_at\", \"lane\", \"latest\", \"version\", \"origin\", \"encoding\", \"report\" FROM \"_quadra_inbox\"";

/// Messages received and not handled yet whose time has come (`now`), oldest first, at most
/// `limit`; of an `ordered by`, only the first of each value not done.
pub fn to_handle(conn: &mut Conn, now: &str, limit: usize) -> Result<Vec<Message>, String> {
    Ok(conn
        .query(
            &format!("{MESSAGE} WHERE \"state\" = 'received' AND (\"next_at\" IS NULL OR \"next_at\" <= ?text) AND (\"lane\" IS NULL OR NOT EXISTS (SELECT 1 FROM \"_quadra_inbox\" p WHERE p.\"inbound\" = \"_quadra_inbox\".\"inbound\" AND p.\"lane\" = \"_quadra_inbox\".\"lane\" AND p.\"id\" < \"_quadra_inbox\".\"id\" AND p.\"state\" IN ('received', 'rejected'))) ORDER BY \"_quadra_inbox\".\"id\" LIMIT {limit}"),
            &[Sql::Text(now.to_string())],
        )?
        .iter()
        .map(|r| message_of(r))
        .collect())
}

/// The messages of the inbox, oldest first (in `state` only, if given).
pub fn inbox(conn: &mut Conn, state: Option<&str>) -> Result<Vec<Message>, String> {
    let (filter, params) = match state {
        Some(s) => (" WHERE \"state\" = ?text", vec![Sql::Text(s.to_string())]),
        None => ("", vec![]),
    };
    Ok(conn
        .query(
            &format!("{MESSAGE}{filter} ORDER BY \"_quadra_inbox\".\"id\""),
            &params,
        )?
        .iter()
        .map(|r| message_of(r))
        .collect())
}

/// What came of handling message `id`: done by transaction `tx`, or rejected with the error.
pub fn handled(
    conn: &mut Conn,
    id: u64,
    name: &str,
    now: &str,
    result: Result<u64, &str>,
) -> Result<(), String> {
    match result {
        Ok(tx) => {
            conn.exec(
                "UPDATE \"_quadra_inbox\" SET \"state\" = 'done', \"done_at\" = ?text, \"tx\" = ?bigint, \"attempts\" = \"attempts\" + 1, \"error\" = NULL WHERE \"id\" = ?bigint AND \"state\" = 'received'",
                &[Sql::Text(now.to_string()), Sql::Int(tx as i64), Sql::Int(id as i64)],
            )?;
            log_dir(
                conn,
                "in",
                id,
                name,
                now,
                "handle",
                "done",
                None,
                None,
                None,
                Some(&format!("transaction {tx}")),
            )
        }
        Err(e) => {
            conn.exec(
                "UPDATE \"_quadra_inbox\" SET \"state\" = 'rejected', \"attempts\" = \"attempts\" + 1, \"error\" = ?text WHERE \"id\" = ?bigint AND \"state\" = 'received'",
                &[Sql::Text(e.to_string()), Sql::Int(id as i64)],
            )?;
            log_dir(
                conn,
                "in",
                id,
                name,
                now,
                "handle",
                "rejected",
                None,
                Some(e),
                None,
                None,
            )
        }
    }
}

/// Messages rejected or skipped, received again to be handled by the code deployed now
/// (`quadra inbox retry`). Returns how many.
pub fn inbox_retry(conn: &mut Conn, ids: &[u64], now: &str, by: &str) -> Result<usize, String> {
    let mut n = 0;
    for id in ids {
        let done = conn.exec(
            "UPDATE \"_quadra_inbox\" SET \"state\" = 'received', \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"state\" IN ('rejected', 'skipped', 'superseded')",
            &[Sql::Int(*id as i64)],
        )?;
        if done > 0 {
            let name = inbox_name(conn, *id)?;
            log_dir(
                conn,
                "in",
                *id,
                &name,
                now,
                "retry",
                "received",
                None,
                None,
                Some(by),
                None,
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Messages not to be handled, with a reason (`quadra inbox skip`). Returns how many.
pub fn inbox_skip(
    conn: &mut Conn,
    ids: &[u64],
    now: &str,
    by: &str,
    reason: &str,
) -> Result<usize, String> {
    if reason.trim().is_empty() {
        return Err("a skip needs a reason (--reason \"…\")".into());
    }
    let mut n = 0;
    for id in ids {
        let done = conn.exec(
            "UPDATE \"_quadra_inbox\" SET \"state\" = 'skipped' WHERE \"id\" = ?bigint AND \"state\" IN ('received', 'rejected')",
            &[Sql::Int(*id as i64)],
        )?;
        if done > 0 {
            let name = inbox_name(conn, *id)?;
            log_dir(
                conn,
                "in",
                *id,
                &name,
                now,
                "skip",
                "skipped",
                None,
                None,
                Some(by),
                Some(reason),
            )?;
            n += 1;
        }
    }
    Ok(n)
}

fn inbox_name(conn: &mut Conn, id: u64) -> Result<String, String> {
    Ok(conn
        .query(
            "SELECT \"inbound\" FROM \"_quadra_inbox\" WHERE \"id\" = ?bigint",
            &[Sql::Int(id as i64)],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].clone())
        .unwrap_or_default())
}

/// The answer already given to a request of an endpoint with this key (`POST`): the same
/// request sent again gets the same answer, and is not handled twice.
pub fn answered(conn: &mut Conn, name: &str, key: &str) -> Result<Option<String>, String> {
    Ok(conn
        .query(
            "SELECT \"response\" FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"key\" = ?text AND \"state\" = 'done' ORDER BY \"id\" LIMIT 1",
            &[Sql::Text(name.to_string()), Sql::Text(key.to_string())],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].clone()))
}

/// The answer of a request handled at once (an endpoint's `POST`), kept with it.
pub fn respond(conn: &mut Conn, id: u64, response: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_inbox\" SET \"response\" = ?text WHERE \"id\" = ?bigint",
        &[Sql::Text(response.to_string()), Sql::Int(id as i64)],
    )?;
    Ok(())
}

/// Where the pull of `inbound` is: its cursor and when it pulls next (none: never pulled).
pub fn pull_state(
    conn: &mut Conn,
    inbound: &str,
) -> Result<(Option<String>, Option<String>), String> {
    Ok(conn
        .query(
            "SELECT \"cursor\", \"next_at\" FROM \"_quadra_cursors\" WHERE \"inbound\" = ?text",
            &[Sql::Text(inbound.to_string())],
        )?
        .into_iter()
        .next()
        .map(|r| (r[0].clone(), r[1].clone()))
        .unwrap_or((None, None)))
}

/// A pull done at `now`: the next at `next_at`, its new cursor (kept as it was when `None`),
/// the error if it failed. Written with the messages it brought, in one batch.
pub fn pulled(
    conn: &mut Conn,
    inbound: &str,
    now: &str,
    next_at: &str,
    cursor: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    let (old, _) = pull_state(conn, inbound)?;
    let cursor = cursor.map(|c| c.to_string()).or(old);
    conn.exec(
        "INSERT INTO \"_quadra_cursors\" (\"inbound\", \"cursor\", \"pulled_at\", \"next_at\", \"error\") VALUES (?text, ?text, ?text, ?text, ?text) ON CONFLICT (\"inbound\") DO UPDATE SET \"cursor\" = excluded.\"cursor\", \"pulled_at\" = excluded.\"pulled_at\", \"next_at\" = excluded.\"next_at\", \"error\" = excluded.\"error\"",
        &[
            Sql::Text(inbound.to_string()),
            cursor.map(Sql::Text).unwrap_or(Sql::Null),
            Sql::Text(now.to_string()),
            Sql::Text(next_at.to_string()),
            error.map(|e| Sql::Text(e.to_string())).unwrap_or(Sql::Null),
        ],
    )?;
    if let Some(e) = error {
        log_dir(
            conn,
            "in",
            0,
            inbound,
            now,
            "pull",
            "failed",
            None,
            Some(e),
            None,
            None,
        )?;
    }
    Ok(())
}

/// The transaction that already handled message `id` (`inbound:Name#id`), if one did: a
/// message is handled once even if the server stopped between the transaction and writing it.
pub fn handled_by(conn: &mut Conn, name: &str, id: u64) -> Result<Option<u64>, String> {
    Ok(conn
        .query(
            "SELECT CAST(\"tx\" AS TEXT) FROM \"_quadra_commits\" WHERE \"cause\" LIKE ?text ORDER BY \"tx\" DESC LIMIT 1",
            &[Sql::Text(format!("%inbound:{name}#{id} %"))],
        )?
        .into_iter()
        .next()
        .and_then(|r| r[0].as_deref().and_then(|v| v.parse().ok())))
}

/// The rows of message `id` of `inbound` committed by chunks (`atomic per row`): the ranges of
/// their lines (`inbound:Name#id/lines 2-501 handled`, `…/line 7 handled`), and the last of
/// those transactions. A server stopped halfway through a file goes on after them.
#[allow(clippy::type_complexity)]
pub fn rows_handled(
    conn: &mut Conn,
    name: &str,
    id: u64,
) -> Result<(Vec<(usize, usize)>, Option<u64>), String> {
    let mark = format!("inbound:{name}#{id}/");
    let mut ranges = Vec::new();
    let mut last = None;
    for r in conn.query(
        "SELECT CAST(\"tx\" AS TEXT), \"cause\" FROM \"_quadra_commits\" WHERE \"cause\" LIKE ?text ORDER BY \"tx\"",
        &[Sql::Text(format!("%{mark}%"))],
    )? {
        let (Some(tx), Some(cause)) = (r[0].as_deref(), r[1].as_deref()) else {
            continue;
        };
        let Some(rest) = cause.split(&mark).nth(1) else {
            continue;
        };
        let span = rest.split_whitespace().nth(1).unwrap_or_default();
        let (a, b) = span.split_once('-').unwrap_or((span, span));
        if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
            ranges.push((a, b));
            last = tx.parse().ok().or(last);
        }
    }
    Ok((ranges, last))
}

/// The next pull of `inbound` brought forward to `now` once none of its messages waits to be
/// handled: what was handled is settled on the other side (a file moved to `processed/`) at
/// once rather than at the next pull.
pub fn pull_soon(conn: &mut Conn, inbound: &str, now: &str) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_cursors\" SET \"next_at\" = ?text WHERE \"inbound\" = ?text AND \"next_at\" > ?text AND NOT EXISTS (SELECT 1 FROM \"_quadra_inbox\" WHERE \"inbound\" = ?text AND \"state\" = 'received')",
        &[
            Sql::Text(now.to_string()),
            Sql::Text(inbound.to_string()),
            Sql::Text(now.to_string()),
            Sql::Text(inbound.to_string()),
        ],
    )?;
    Ok(())
}

/// A message received again (a file that came back with the same content): settled again
/// on the other side, as the first time.
pub fn unsettle(conn: &mut Conn, id: u64) -> Result<(), String> {
    conn.exec(
        "UPDATE \"_quadra_inbox\" SET \"settled_at\" = NULL WHERE \"id\" = ?bigint AND \"origin\" IS NOT NULL",
        &[Sql::Int(id as i64)],
    )?;
    Ok(())
}

/// How long to wait after the `attempts`-th failed attempt: 30 s, doubled each time, at most
/// an hour; `jitter` in 0..1 moves it by up to a fifth either way, so that many events that
/// failed together are not all tried again at once.
pub fn backoff(attempts: u64, jitter: f64) -> std::time::Duration {
    backoff_between(attempts, jitter, 30, 3600)
}

/// The same, from `first_s` doubled up to `max_s` (`backoff 10 seconds .. 5 minutes`).
pub fn backoff_between(
    attempts: u64,
    jitter: f64,
    first_s: u64,
    max_s: u64,
) -> std::time::Duration {
    let base = first_s
        .max(1)
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(30))
        .min(max_s.max(first_s));
    let factor = 0.8 + 0.4 * jitter.clamp(0.0, 1.0);
    std::time::Duration::from_secs_f64(base as f64 * factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_up_to_an_hour() {
        assert_eq!(backoff(1, 0.5).as_secs(), 30);
        assert_eq!(backoff(2, 0.5).as_secs(), 60);
        assert_eq!(backoff(3, 0.5).as_secs(), 120);
        assert_eq!(backoff(10, 0.5).as_secs(), 3600);
        assert_eq!(backoff(100, 0.5).as_secs(), 3600);
        assert_eq!(backoff(1, 0.0).as_secs(), 24);
        assert_eq!(backoff(1, 1.0).as_secs(), 36);
    }
}

// ---- how long the messages and the events are kept ------------------------------------------

/// How long the closed messages of one `inbound` (or the closed events of one `outbound`)
/// are kept: those closed before `before` go (none: kept for ever); `archive`: written to an
/// archive in the store of files first. The rows of the messages received stay thin (key,
/// hash, state) until `thin_before`, so that a message sent again is still known.
#[derive(Clone, Debug)]
pub struct Keep {
    pub name: String,
    pub before: Option<String>,
    pub thin_before: Option<String>,
    pub archive: bool,
}

/// Unless the project says otherwise (`[data] inbox`, `outbox`): 90 days and 30 days.
pub const INBOX_DAYS: u32 = 90;
pub const OUTBOX_DAYS: u32 = 30;

fn keep_of(
    name: &str,
    own: Option<&quadra_sema::manifest::TermDecl>,
    project: Option<&quadra_sema::manifest::TermDecl>,
    days: u32,
    now: chrono::NaiveDateTime,
) -> Keep {
    use quadra_sema::manifest::TermDecl;
    let fallback = TermDecl::For {
        n: days,
        unit: "days".into(),
        archive: false,
    };
    let t = own.or(project).unwrap_or(&fallback);
    let stamp = |t: chrono::NaiveDateTime| t.format("%Y-%m-%dT%H:%M:%S").to_string();
    match t {
        TermDecl::Forever => Keep {
            name: name.to_string(),
            before: None,
            thin_before: None,
            archive: false,
        },
        TermDecl::For { n, unit, archive } => {
            let term = quadra_ir::Term {
                n: *n as i64,
                unit: unit.clone(),
                after: String::new(),
                span: quadra_ir::Span {
                    file: quadra_sema::FileId(0),
                    range: Default::default(),
                },
            };
            let before = term.before(now);
            Keep {
                name: name.to_string(),
                before: before.map(stamp),
                thin_before: before.and_then(|b| term.before(b)).map(stamp),
                archive: *archive,
            }
        }
    }
}

/// The terms of the project's messages (`inbound … keep …`, `[data] inbox`) and events
/// (`outbound … keep …`, `[data] outbox`), as of `now`.
pub fn message_terms(
    model: &quadra_ir::Model,
    now: chrono::NaiveDateTime,
) -> (Vec<Keep>, Vec<Keep>) {
    let d = &model.manifest.data;
    let inbox = model
        .inbounds
        .iter()
        .map(|i| keep_of(&i.name, i.keep.as_ref(), d.inbox.as_ref(), INBOX_DAYS, now))
        .collect();
    let mut outbox: Vec<Keep> = Vec::new();
    for o in &model.outbounds {
        if outbox.iter().any(|k| k.name == o.event) {
            continue;
        }
        outbox.push(keep_of(
            &o.event,
            o.keep.as_ref(),
            d.outbox.as_ref(),
            OUTBOX_DAYS,
            now,
        ));
    }
    (inbox, outbox)
}

/// A message received is closed (handled, skipped, superseded; a file settled), and not the
/// latest of its lane (`latest by` compares with it).
const INBOX_CLOSED: &str = "\"inbound\" = ?text AND \"state\" IN ('done', 'skipped', 'superseded') AND COALESCE(\"done_at\", \"received_at\") < ?text AND (\"origin\" IS NULL OR \"settled_at\" IS NOT NULL) AND (\"lane\" IS NULL OR \"id\" < (SELECT MAX(i2.\"id\") FROM \"_quadra_inbox\" i2 WHERE i2.\"inbound\" = \"_quadra_inbox\".\"inbound\" AND i2.\"lane\" = \"_quadra_inbox\".\"lane\"))";

/// An event is closed (delivered, skipped, superseded), and not the latest of its lane.
const OUTBOX_CLOSED: &str = "\"event\" = ?text AND \"state\" IN ('delivered', 'skipped', 'superseded') AND COALESCE(\"delivered_at\", \"created_at\") < ?text AND (\"lane\" IS NULL OR \"id\" < (SELECT MAX(o2.\"id\") FROM \"_quadra_outbox\" o2 WHERE o2.\"event\" = \"_quadra_outbox\".\"event\" AND o2.\"lane\" = \"_quadra_outbox\".\"lane\"))";

/// The messages and events whose term ended and that go to an archive first (`keep …, then
/// archive`), as JSON lines: `("inbox" | "outbox", row)`, at most `limit`.
pub fn to_archive(
    conn: &mut Conn,
    inbox: &[Keep],
    outbox: &[Keep],
    limit: usize,
) -> Result<Vec<(String, J)>, String> {
    let mut out = Vec::new();
    for k in inbox.iter().filter(|k| k.archive) {
        let Some(before) = &k.before else { continue };
        for r in conn.query(
            &format!("SELECT CAST(\"id\" AS TEXT), \"key\", \"hash\", \"body\", \"received_at\", \"state\", \"done_at\", CAST(\"tx\" AS TEXT), \"response\" FROM \"_quadra_inbox\" WHERE {INBOX_CLOSED} AND \"body\" <> '' ORDER BY \"_quadra_inbox\".\"id\" LIMIT {limit}"),
            &[Sql::Text(k.name.clone()), Sql::Text(before.clone())],
        )? {
            out.push((
                "inbox".to_string(),
                serde_json::json!({"inbound": k.name, "id": r[0], "key": r[1], "hash": r[2], "body": r[3], "received_at": r[4], "state": r[5], "done_at": r[6], "tx": r[7], "response": r[8]}),
            ));
        }
    }
    for k in outbox.iter().filter(|k| k.archive) {
        let Some(before) = &k.before else { continue };
        for r in conn.query(
            &format!("SELECT CAST(\"id\" AS TEXT), CAST(\"tx\" AS TEXT), \"fields\", \"created_at\", \"state\", \"delivered_at\", CAST(\"attempts\" AS TEXT) FROM \"_quadra_outbox\" WHERE {OUTBOX_CLOSED} ORDER BY \"_quadra_outbox\".\"id\" LIMIT {limit}"),
            &[Sql::Text(k.name.clone()), Sql::Text(before.clone())],
        )? {
            out.push((
                "outbox".to_string(),
                serde_json::json!({"event": k.name, "id": r[0], "tx": r[1], "fields": r[2], "created_at": r[3], "state": r[4], "delivered_at": r[5], "attempts": r[6]}),
            ));
        }
    }
    Ok(out)
}

/// The closed messages and events whose term ended: the body and the answer of a message
/// emptied (its thin row deleted once `thin_before` passed too), an event deleted. Returns
/// (messages emptied, rows deleted).
pub fn prune_messages(
    conn: &mut Conn,
    inbox: &[Keep],
    outbox: &[Keep],
) -> Result<(usize, usize), String> {
    let (mut emptied, mut deleted) = (0, 0);
    for k in inbox {
        if let Some(thin) = &k.thin_before {
            deleted += conn.exec(
                &format!("DELETE FROM \"_quadra_inbox\" WHERE {INBOX_CLOSED}"),
                &[Sql::Text(k.name.clone()), Sql::Text(thin.clone())],
            )? as usize;
        }
        if let Some(before) = &k.before {
            emptied += conn.exec(
                &format!("UPDATE \"_quadra_inbox\" SET \"body\" = '', \"response\" = NULL, \"report\" = NULL WHERE {INBOX_CLOSED} AND \"body\" <> ''"),
                &[Sql::Text(k.name.clone()), Sql::Text(before.clone())],
            )? as usize;
        }
    }
    for k in outbox {
        if let Some(before) = &k.before {
            deleted += conn.exec(
                &format!("DELETE FROM \"_quadra_outbox\" WHERE {OUTBOX_CLOSED}"),
                &[Sql::Text(k.name.clone()), Sql::Text(before.clone())],
            )? as usize;
        }
    }
    Ok((emptied, deleted))
}
