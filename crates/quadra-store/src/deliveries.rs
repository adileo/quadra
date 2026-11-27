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
        &format!("SELECT CAST(o.\"id\" AS TEXT), CAST(o.\"tx\" AS TEXT), o.\"event\", o.\"fields\", o.\"created_at\", CAST(o.\"attempts\" AS TEXT), c.\"hash\" FROM \"_quadra_outbox\" o LEFT JOIN \"_quadra_commits\" c ON c.\"tx\" = o.\"tx\" WHERE o.\"delivered_at\" IS NULL AND o.\"failed_at\" IS NULL AND o.\"event\" IN ({marks}) AND (o.\"next_at\" IS NULL OR o.\"next_at\" <= ?text) AND o.\"tx\" <= ?bigint ORDER BY o.\"id\" LIMIT {limit}"),
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
        &format!("SELECT COUNT(*), MIN(COALESCE(\"next_at\", '')) FROM \"_quadra_outbox\" WHERE \"delivered_at\" IS NULL AND \"failed_at\" IS NULL AND \"event\" IN ({marks})"),
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

/// Writes what came of an attempt on event `id` at `now`. An event already delivered or
/// failed is left as it is.
pub fn record(conn: &mut Conn, id: u64, now: &str, outcome: &Outcome) -> Result<(), String> {
    let id = Sql::Int(id as i64);
    let now = Sql::Text(now.to_string());
    let pending = "\"id\" = ?bigint AND \"delivered_at\" IS NULL AND \"failed_at\" IS NULL";
    match outcome {
        Outcome::Delivered => conn.exec(
            &format!("UPDATE \"_quadra_outbox\" SET \"delivered\" = ?boolean, \"delivered_at\" = ?text, \"attempts\" = \"attempts\" + 1, \"next_at\" = NULL, \"error\" = NULL WHERE {pending}"),
            &[Sql::Bool(true), now, id],
        )?,
        Outcome::Retry { at, error } => conn.exec(
            &format!("UPDATE \"_quadra_outbox\" SET \"attempts\" = \"attempts\" + 1, \"next_at\" = ?text, \"error\" = ?text WHERE {pending}"),
            &[Sql::Text(at.clone()), Sql::Text(error.clone()), id],
        )?,
        Outcome::Failed { error } => conn.exec(
            &format!("UPDATE \"_quadra_outbox\" SET \"attempts\" = \"attempts\" + 1, \"failed_at\" = ?text, \"error\" = ?text WHERE {pending}"),
            &[now, Sql::Text(error.clone()), id],
        )?,
    };
    Ok(())
}

/// Events that failed for good made pending again (`quadra outbox retry`): tried at the next
/// round. Returns how many.
pub fn retry(conn: &mut Conn, ids: &[u64]) -> Result<usize, String> {
    let mut n = 0;
    for id in ids {
        n += conn.exec(
            "UPDATE \"_quadra_outbox\" SET \"failed_at\" = NULL, \"next_at\" = NULL WHERE \"id\" = ?bigint AND \"delivered_at\" IS NULL AND \"failed_at\" IS NOT NULL",
            &[Sql::Int(*id as i64)],
        )? as usize;
    }
    Ok(n)
}

/// How long to wait after the `attempts`-th failed attempt: 30 s, doubled each time, at most
/// an hour; `jitter` in 0..1 moves it by up to a fifth either way, so that many events that
/// failed together are not all tried again at once.
pub fn backoff(attempts: u64, jitter: f64) -> std::time::Duration {
    let base = 30u64
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(20))
        .min(3600);
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
