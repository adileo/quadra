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

/// A credential of its own for client `n` (a credential's requests per second are limited).
fn client_token(url: &str, n: u64) -> String {
    let now = chrono::Local::now().naive_local();
    let mut c = Conn::open(url).unwrap();
    quadra_store::auth::create(&mut c, "token", 1, &format!("client {n}"), false, now, None)
        .unwrap()
        .0
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

const JOIN: &str = r#"
entity Order:
    number: Text
    paid: Bool default false
    approved: Bool default false
    picked: Int default 0
    lines: [Line]

entity Line:
    order: Order parent
    sku: Text

entity Shipment:
    order: Order

event OrderReady(number: Text)

on Order becomes paid and approved and picked >= 40 and lines.count() >= 40 as o:
    Shipment(order=o)
    emit OrderReady(number=o.number)
"#;

/// What the clients of the stress test met.
#[derive(Default, Debug, Clone, Copy)]
struct Tally {
    committed: u64,
    conflicts: u64,
    /// `429`: past the requests per second of a credential, waited and sent again
    throttled: u64,
}

impl Tally {
    fn add(&mut self, o: Tally) {
        self.committed += o.committed;
        self.conflicts += o.conflicts;
        self.throttled += o.throttled;
    }
}

/// A request as a client sends it: on `429` it waits a little and sends it again.
fn send(port: u16, host: &str, token: &str, path: &str, body: J, t: &mut Tally) -> (u16, J) {
    loop {
        let (s, r) = http(port, host, token, "POST", path, Some(body.clone()));
        if s != 429 {
            return (s, r);
        }
        t.throttled += 1;
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Writes `values` on a record as a client would: read it (its `base`), send the change from
/// that `base`, and on a `409` read again and send it again with a new key.
fn write_from_base(
    port: u16,
    host: &str,
    token: &str,
    entity: &str,
    id: u64,
    values: impl Fn(&J) -> J,
) -> Tally {
    let mut t = Tally::default();
    loop {
        let q = json!({ "entity": entity, "ids": [id] });
        let (s, r) = send(port, host, token, "/v1/query", q, &mut t);
        assert_eq!(s, 200, "{r}");
        let base = r["base"].as_u64().unwrap();
        let v = values(&r["rows"][0]);
        let tx = json!({ "base": base, "ops": [{ "op": "update", "entity": entity, "id": id, "values": v }] });
        let (s, r) = send(port, host, token, "/v1/tx", tx, &mut t);
        match s {
            200 => {
                t.committed += 1;
                return t;
            }
            409 => t.conflicts += 1,
            _ => panic!("{s}: {r}"),
        }
        assert!(t.conflicts < 1000, "never got through");
    }
}

/// Many clients at once on the inputs of one `becomes` (`paid and approved and picked >= 40
/// and lines.count() >= 40`): counters raised from a `base` with retries on `409`, lines
/// created, the two flags set, all racing. Nothing is lost, and the reaction fires exactly
/// once per order: one Shipment, one event in the outbox.
#[test]
fn many_clients_racing_on_one_becomes_fire_it_once() {
    const PICKERS: u64 = 8;
    const PICKS: u64 = 10;
    const LINERS: u64 = 8;
    const LINES: u64 = 10;
    const ORDERS: u64 = 3;
    let (cfg, token) = project(&temp("orch-race"), JOIN);
    let url = cfg.url.clone();
    let (host, rt, port) = serve(vec![(
        "j.test",
        TenantSpec {
            id: "j".into(),
            config: cfg,
        },
    )]);
    let clients = PICKERS + LINERS + 2;
    let tokens: Vec<String> = (0..clients).map(|n| client_token(&url, n)).collect();
    let started = Instant::now();
    let mut tally = Tally::default();
    let mut ids = Vec::new();
    for n in 0..ORDERS {
        let (s, r) = http(
            port,
            "j.test",
            &token,
            "POST",
            "/v1/tx",
            Some(
                json!({ "ops": [{ "op": "create", "entity": "Order", "ref": "o", "values": { "number": format!("O{n}") } }] }),
            ),
        );
        assert_eq!(s, 200, "{r}");
        let id = r["created"]["o"].as_u64().unwrap();
        ids.push(id);
        let go = std::sync::Arc::new(std::sync::Barrier::new(clients as usize));
        let mut threads = Vec::new();
        let mut tokens = tokens.iter().cloned();
        for _ in 0..PICKERS {
            let (go, token) = (go.clone(), tokens.next().unwrap());
            threads.push(std::thread::spawn(move || {
                go.wait();
                let mut t = Tally::default();
                for _ in 0..PICKS {
                    t.add(write_from_base(
                        port,
                        "j.test",
                        &token,
                        "Order",
                        id,
                        |row| json!({ "picked": cell(row, "picked").as_i64().unwrap() + 1 }),
                    ));
                }
                t
            }));
        }
        for w in 0..LINERS {
            let (go, token) = (go.clone(), tokens.next().unwrap());
            threads.push(std::thread::spawn(move || {
                go.wait();
                let mut t = Tally::default();
                for l in 0..LINES {
                    let tx = json!({ "ops": [{ "op": "create", "entity": "Line", "values": { "order": id, "sku": format!("{w}-{l}") } }] });
                    let (s, r) = send(port, "j.test", &token, "/v1/tx", tx, &mut t);
                    assert_eq!(s, 200, "{r}");
                    t.committed += 1;
                }
                t
            }));
        }
        for flag in ["paid", "approved"] {
            let (go, token) = (go.clone(), tokens.next().unwrap());
            threads.push(std::thread::spawn(move || {
                go.wait();
                write_from_base(
                    port,
                    "j.test",
                    &token,
                    "Order",
                    id,
                    |_| json!({ flag: true }),
                )
            }));
        }
        for t in threads {
            tally.add(t.join().unwrap());
        }
    }
    let took = started.elapsed();
    for (n, id) in ids.iter().enumerate() {
        let o = &rows(
            port,
            "j.test",
            &token,
            json!({ "entity": "Order", "ids": [id] }),
        )[0];
        assert_eq!(
            cell(o, "picked").as_u64(),
            Some(PICKERS * PICKS),
            "a raise was lost: {o}"
        );
        assert_eq!(cell(o, "paid"), &json!(true));
        assert_eq!(cell(o, "approved"), &json!(true));
        let lines = rows(
            port,
            "j.test",
            &token,
            json!({ "entity": "Line", "refs": { "order": id }, "limit": 1000 }),
        );
        assert_eq!(lines.len() as u64, LINERS * LINES);
        let shipments = rows(
            port,
            "j.test",
            &token,
            json!({ "entity": "Shipment", "refs": { "order": id } }),
        );
        assert_eq!(shipments.len(), 1, "order O{n}: {shipments:?}");
    }
    assert!(rt.block_on(host.stop("j")));
    let events = sql(
        &url,
        "SELECT \"fields\" FROM \"_quadra_outbox\" WHERE \"event\" = 'OrderReady' ORDER BY \"id\"",
    );
    assert_eq!(events.len() as u64, ORDERS, "{events:?}");
    for n in 0..ORDERS {
        assert!(
            events
                .iter()
                .any(|e| e[0].as_deref().unwrap_or("").contains(&format!("\"O{n}\""))),
            "{events:?}"
        );
    }
    eprintln!(
        "{} transactions committed in {took:?} ({:.0}/s), {} conflicts (409) and {} throttled (429) sent again",
        tally.committed,
        tally.committed as f64 / took.as_secs_f64(),
        tally.conflicts,
        tally.throttled
    );
    assert!(
        tally.conflicts > 0,
        "no client ever met a conflict: not a race"
    );
}

/// Clients raising one counter at once, each from the `base` it read: a client whose record was
/// changed since gets a `409` and reads again, so no raise is lost. (With several engines
/// writing at once, a transaction run again because another commit landed meanwhile was not
/// checked against its client's `base` again, and overwrote that commit.)
#[test]
fn raises_from_a_base_racing_on_one_record_are_never_lost() {
    const CLIENTS: u64 = 8;
    const RAISES: u64 = 10;
    let (cfg, token) = project(&temp("orch-raises"), JOIN);
    let url = cfg.url.clone();
    let (_host, _rt, port) = serve(vec![(
        "r.test",
        TenantSpec {
            id: "r".into(),
            config: cfg,
        },
    )]);
    let (s, r) = http(
        port,
        "r.test",
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "ref": "o", "values": { "number": "R" } }] }),
        ),
    );
    assert_eq!(s, 200, "{r}");
    let id = r["created"]["o"].as_u64().unwrap();
    let go = std::sync::Arc::new(std::sync::Barrier::new(CLIENTS as usize));
    let clients: Vec<_> = (0..CLIENTS)
        .map(|n| {
            let token = client_token(&url, n);
            let go = go.clone();
            std::thread::spawn(move || {
                go.wait();
                let mut t = Tally::default();
                for _ in 0..RAISES {
                    t.add(write_from_base(
                        port,
                        "r.test",
                        &token,
                        "Order",
                        id,
                        |row| json!({ "picked": cell(row, "picked").as_i64().unwrap() + 1 }),
                    ));
                }
                t
            })
        })
        .collect();
    let mut tally = Tally::default();
    for c in clients {
        tally.add(c.join().unwrap());
    }
    let o = &rows(
        port,
        "r.test",
        &token,
        json!({ "entity": "Order", "ids": [id] }),
    )[0];
    assert_eq!(
        cell(o, "picked").as_u64(),
        Some(CLIENTS * RAISES),
        "raises lost: {tally:?}"
    );
    assert!(tally.conflicts > 0, "no client met a conflict: not a race");
}

const FANOUT: &str = r#"
entity Order:
    number: Text
    lines: [Line]

    action pick_all:
        for l in lines:
            l.picked = true

entity Line:
    order: Order parent
    sku: Text
    picked: Bool default false

entity Shipment:
    order: Order

entity Batch:
    size: Int
    released: Bool default false
    items: [Item]

entity Item:
    batch: Batch parent
    n: Int

on Order becomes lines.count() > 0 and lines.all(l => l.picked) as o:
    Shipment(order=o)

on Batch becomes released as b:
    for n in 1 .. b.size:
        Item(batch=b, n=n)
"#;

/// How many records of `entity` match, counted exactly (`total` is an estimate past 10 000).
fn total(port: u16, host: &str, token: &str, entity: &str, field: &str, refs: J) -> u64 {
    let q = json!({ "entity": entity, "refs": refs, "limit": 1, "count": false, "aggregates": { field: "count" } });
    let (s, r) = http(port, host, token, "POST", "/v1/query", Some(q));
    assert_eq!(s, 200, "{r}");
    r["aggregates"][field]["v"]
        .as_u64()
        .or_else(|| {
            r["aggregates"][field]["v"]
                .as_str()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or_else(|| panic!("{r}"))
}

/// One transaction touching many records, within the default limits of a request: an order
/// created with its lines in one request, an action changing every line (and the join on them
/// firing once), a reaction creating as many records. The times are printed
/// (`QUADRA_FANOUT=50000 cargo test --release … fan_out -- --nocapture` for other sizes).
#[test]
fn a_fan_out_of_ten_thousand_records_in_one_transaction() {
    let n: u64 = std::env::var("QUADRA_FANOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let (cfg, token) = project(&temp("orch-fanout"), FANOUT);
    let (_host, _rt, port) = serve(vec![(
        "f.test",
        TenantSpec {
            id: "f".into(),
            config: cfg,
        },
    )]);
    let tx = |body: J| {
        let t = Instant::now();
        let (s, r) = http(port, "f.test", &token, "POST", "/v1/tx", Some(body));
        assert_eq!(s, 200, "{r}");
        (r, t.elapsed())
    };
    // an order and its n lines, in one request
    let lines: Vec<J> = (0..n).map(|i| json!({ "sku": format!("L{i}") })).collect();
    let body = json!({ "ops": [{ "op": "create", "entity": "Order", "ref": "o", "values": { "number": "big", "lines": lines } }] });
    let bytes = body.to_string().len();
    let (r, created) = tx(body);
    let order = r["created"]["o"].as_u64().unwrap();
    assert_eq!(
        total(
            port,
            "f.test",
            &token,
            "Line",
            "sku",
            json!({ "order": order })
        ),
        n
    );
    // every line changed by one action; the join on all of them fires once
    let base = r["base"].as_u64().unwrap();
    let (_, picked) = tx(
        json!({ "base": base, "ops": [{ "op": "action", "entity": "Order", "id": order, "action": "pick_all" }] }),
    );
    assert_eq!(
        total(
            port,
            "f.test",
            &token,
            "Shipment",
            "order",
            json!({ "order": order })
        ),
        1
    );
    // a reaction creating n records
    let (r, _) = tx(
        json!({ "ops": [{ "op": "create", "entity": "Batch", "ref": "b", "values": { "size": n } }] }),
    );
    let batch = r["created"]["b"].as_u64().unwrap();
    let base = r["base"].as_u64().unwrap();
    let (_, released) = tx(
        json!({ "base": base, "ops": [{ "op": "update", "entity": "Batch", "id": batch, "values": { "released": true } }] }),
    );
    assert_eq!(
        total(
            port,
            "f.test",
            &token,
            "Item",
            "n",
            json!({ "batch": batch })
        ),
        n
    );
    eprintln!(
        "{n} records: created with their order in {created:?} ({} KB of request), changed by one action in {picked:?}, created by one reaction in {released:?}",
        bytes / 1024
    );
}

/// Past the time a request may take, a fan-out fails as a whole: the answer says the limit,
/// nothing of it is written, and the tenant goes on serving.
#[test]
fn a_fan_out_past_the_time_limit_writes_nothing() {
    let (cfg, token) = project(&temp("orch-fanout-limit"), FANOUT);
    let cfg = quadra_server::Config {
        limits: quadra_server::Limits {
            job: Some(Duration::from_millis(300)),
            ..quadra_server::Limits::default()
        },
        ..cfg
    };
    let (_host, _rt, port) = serve(vec![(
        "l.test",
        TenantSpec {
            id: "l".into(),
            config: cfg,
        },
    )]);
    let (s, r) = http(
        port,
        "l.test",
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Batch", "ref": "b", "values": { "size": 200_000 } }] }),
        ),
    );
    assert_eq!(s, 200, "{r}");
    let batch = r["created"]["b"].as_u64().unwrap();
    let base = r["base"].as_u64().unwrap();
    let t = Instant::now();
    let (s, r) = http(
        port,
        "l.test",
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "base": base, "ops": [{ "op": "update", "entity": "Batch", "id": batch, "values": { "released": true } }] }),
        ),
    );
    let took = t.elapsed();
    assert_eq!(s, 422, "{r}");
    assert!(r["error"].as_str().unwrap().starts_with("timeout"), "{r}");
    assert!(took < Duration::from_secs(2), "answered after {took:?}");
    assert_eq!(
        total(
            port,
            "l.test",
            &token,
            "Item",
            "n",
            json!({ "batch": batch })
        ),
        0
    );
    let b = &rows(
        port,
        "l.test",
        &token,
        json!({ "entity": "Batch", "ids": [batch] }),
    )[0];
    assert_eq!(cell(b, "released"), &json!(false));
    // a small one still goes through
    let (s, r) = http(
        port,
        "l.test",
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Batch", "ref": "b", "values": { "size": 10, "released": true } }] }),
        ),
    );
    assert_eq!(s, 200, "{r}");
    let small = r["created"]["b"].as_u64().unwrap();
    assert_eq!(
        total(
            port,
            "l.test",
            &token,
            "Item",
            "n",
            json!({ "batch": small })
        ),
        10
    );
}
