//! Workflow orchestration on a real server and SQLite: a 24-hour wait on a tenant put to sleep
//! fires through the sweep its host runs when `next_sweep` says, many clients racing on the
//! inputs of one `becomes` make it fire once, and one transaction may touch many records.

use super::host::{http, project, serve, temp};
use quadra_server::TenantSpec;
use quadra_store::Conn;
use serde_json::{json, Value as J};
use std::time::{Duration, Instant};

fn utc(d: chrono::Duration) -> String {
    (chrono::Utc::now().naive_utc() + d)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn rows(port: u16, host: &str, token: &str, query: J) -> Vec<J> {
    let (s, r) = http(port, host, token, "POST", "/v1/query", Some(query));
    assert_eq!(s, 200, "{r}");
    r["rows"].as_array().cloned().unwrap_or_default()
}

/// The value of a cell of a row of `/v1/query`.
fn cell<'a>(row: &'a J, field: &str) -> &'a J {
    let c = &row["cells"][field];
    if c.get("v").is_some() {
        &c["v"]
    } else {
        c
    }
}

fn sql(url: &str, q: &str) -> Vec<Vec<Option<String>>> {
    Conn::open(url).unwrap().query(q, &[]).unwrap()
}

const WAIT: &str = r#"
entity Order:
    number: Text
    shipped_at: DateTime

entity Review:
    order: Order

on Order becomes now() >= shipped_at + 24 hours as o:
    Review(order=o)
"#;

/// A tenant asleep (stopped) is woken by its host when `next_sweep` says: the nearest 24-hour
/// wait, not the periodic interval and not a later wait. The sweep then fires that wait once,
/// with no request having touched the order; a sweep before the time fires nothing.
#[test]
fn a_24_hour_wait_fires_on_a_sleeping_tenant_through_the_sweep() {
    let (cfg, token) = project(&temp("orch-wait"), WAIT);
    let url = cfg.url.clone();
    let (host, rt, port) = serve(vec![(
        "w.test",
        TenantSpec {
            id: "w".into(),
            config: cfg,
        },
    )]);
    // shipped 24 hours ago less 4 seconds, and an hour ago
    let soon = utc(chrono::Duration::seconds(4) - chrono::Duration::hours(24));
    let later = utc(-chrono::Duration::hours(1));
    let (s, r) = http(
        port,
        "w.test",
        &token,
        "POST",
        "/v1/tx",
        Some(json!({ "ops": [
            { "op": "create", "entity": "Order", "ref": "soon", "values": { "number": "soon", "shipped_at": soon } },
            { "op": "create", "entity": "Order", "ref": "later", "values": { "number": "later", "shipped_at": later } },
        ] })),
    );
    assert_eq!(s, 200, "{r}");
    let soon_id = r["created"]["soon"].as_u64().unwrap();
    let reviews = |port| {
        rows(
            port,
            "w.test",
            &token,
            json!({ "entity": "Review", "fields": ["order"] }),
        )
    };
    assert!(reviews(port).is_empty());

    // a sweep before the time changes nothing
    assert_eq!(rt.block_on(host.sweep("w.test")), Ok(None));
    // the wake time: the nearer wait, in a few seconds (the periodic sweep is an hour away,
    // the other wait 23 hours away)
    let wake = rt.block_on(host.next_sweep("w.test")).unwrap();
    assert!(wake <= Duration::from_secs(5), "wakes in {wake:?}");
    // asleep: the engine stopped, the database closed
    assert!(rt.block_on(host.stop("w")));
    assert!(host.running().is_empty());
    std::thread::sleep(wake + Duration::from_millis(1100));

    // woken for its sweep: the wait fires, in a transaction of the sweep
    let swept = rt.block_on(host.sweep("w.test")).unwrap();
    assert!(swept.is_some(), "the sweep wrote nothing");
    let cause = sql(
        &url,
        &format!(
            "SELECT \"cause\" FROM \"_quadra_commits\" WHERE \"tx\" = {}",
            swept.unwrap()
        ),
    );
    assert!(
        cause[0][0].as_deref().unwrap_or("").contains("sweep"),
        "{cause:?}"
    );
    let r = reviews(port);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(cell(&r[0], "order")["ref"].as_u64(), Some(soon_id), "{r:?}");
    // once: another sweep, and the next wake is the other wait, 23 hours away
    assert_eq!(rt.block_on(host.sweep("w.test")), Ok(None));
    assert_eq!(reviews(port).len(), 1);
    let wake = rt.block_on(host.next_sweep("w.test")).unwrap();
    assert!(
        wake > Duration::from_secs(22 * 3600) && wake <= Duration::from_secs(23 * 3600),
        "the next wake in {wake:?}"
    );
}

/// An awake tenant left alone (no write after the order) fires a 24-hour wait by its own
/// sweep when the time passes, not at its periodic interval (an hour here).
#[test]
fn a_24_hour_wait_fires_on_an_awake_tenant_by_itself() {
    let (cfg, token) = project(&temp("orch-awake"), WAIT);
    let (_host, _rt, port) = serve(vec![(
        "a.test",
        TenantSpec {
            id: "a".into(),
            config: cfg,
        },
    )]);
    // awake, and past its first look at the clock (nothing due then)
    assert_eq!(http(port, "a.test", &token, "GET", "/v1/me", None).0, 200);
    std::thread::sleep(Duration::from_secs(6));
    let at = utc(chrono::Duration::seconds(3) - chrono::Duration::hours(24));
    let (s, r) = http(
        port,
        "a.test",
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": "1", "shipped_at": at } }] }),
        ),
    );
    assert_eq!(s, 200, "{r}");
    let reviews = || {
        rows(
            port,
            "a.test",
            &token,
            json!({ "entity": "Review", "fields": ["order"] }),
        )
        .len()
    };
    assert_eq!(reviews(), 0);
    let t = Instant::now();
    while reviews() == 0 {
        assert!(
            t.elapsed() < Duration::from_secs(25),
            "the wait never fired"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        t.elapsed() >= Duration::from_secs(2),
        "fired early, after {:?}",
        t.elapsed()
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(reviews(), 1);
}
