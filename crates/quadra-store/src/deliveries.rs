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
        "INSERT INTO \"_quadra_exchanges\" (\"id\", \"at\", \"direction\", \"message\", \"name\", \"connection\", \"step\", \"outcome\", \"status\", \"duration_ms\", \"error\", \"excerpt\", \"by\", \"reason\") VALUES (?bigint, ?text, 'out', ?bigint, ?text, ?text, ?text, ?text, ?bigint, ?bigint, ?text, ?text, ?text, ?text)",
        &[
            Sql::Int(next),
            Sql::Text(at.to_string()),
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
    let rows = conn.query(
        "SELECT \"at\", \"step\", \"outcome\", \"connection\", CAST(\"status\" AS TEXT), CAST(\"duration_ms\" AS TEXT), \"error\", \"excerpt\", \"by\", \"reason\" FROM \"_quadra_exchanges\" WHERE \"direction\" = 'out' AND \"message\" = ?bigint ORDER BY \"_quadra_exchanges\".\"id\"",
        &[Sql::Int(id as i64)],
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
