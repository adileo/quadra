//! The on-premise agent: registered with a one-time code, it takes the jobs of the connections
//! it runs (`location = "agent:<name>"`) over connections it opens itself, and a revoked agent
//! receives nothing more within 5 s.

use crate::deliver::{call, project_at, serve, temp};
use quadra_server::HostOptions;
use quadra_store::Conn;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CODE: &str = "entity Order:\n    number: Text\n\nrecord ErpOrder from json:\n    number: Text at \"NUMERO\"\n\nevent OrderPlaced(order: Order)\n\non Order created as o:\n    emit OrderPlaced(order=o)\n\noutbound OrderPlaced via erp as e:\n    backoff 1 seconds .. 1 seconds\n    insert(\"SCAMBIO_ORDINI\", ErpOrder(number=e.order.number))\n";

const ERP: &str = "\n[vars.ERP_PASSWORD]\nsecret = true\n\n[connections.erp]\nkind = \"postgres\"\nurl = \"postgres://erp.lan:5432/gestionale\"\nuser = \"quadra\"\npassword = { var = \"ERP_PASSWORD\" }\nlocation = \"agent:milano\"\n";

/// An agent running in a thread until `stop`, or until it is revoked (its error, then).
fn agent(
    port: u16,
    key: &str,
    stop: Arc<AtomicBool>,
) -> (
    std::thread::JoinHandle<Result<(), String>>,
    Arc<Mutex<Vec<String>>>,
) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let l = lines.clone();
    let key = key.to_string();
    let h = std::thread::spawn(move || {
        let mut env = std::collections::BTreeMap::new();
        // the credential is the agent's, never the server's
        env.insert("ERP_PASSWORD".to_string(), "only-here".to_string());
        quadra_server::agent::run(
            quadra_server::agent::Options {
                server: format!("http://127.0.0.1:{port}"),
                key,
                env,
                concurrency: 2,
            },
            move |line| l.lock().unwrap().push(line.to_string()),
            stop,
        )
    });
    (h, lines)
}

fn until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "waited for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn an_agent_registers_takes_the_jobs_of_its_connections_and_is_revoked_at_once() {
    let dir = temp("agents");
    // the server has no value for ERP_PASSWORD: the agent has it
    let (cfg, token) = project_at(&dir, "http://127.0.0.1:9", CODE, ERP);
    let (_h, _rt, port) = serve(
        HostOptions {
            public_egress_only: true,
            ..Default::default()
        },
        cfg.clone(),
    );
    // an admin adds it: a one-time code
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "milano" })),
    );
    assert_eq!(s, 200, "{v}");
    let code = v["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 14, "{code}");
    let server = format!("http://127.0.0.1:{port}");
    let (name, key) = quadra_server::agent::register(&server, &code).unwrap();
    assert_eq!(name, "milano");
    assert!(key.starts_with("qa_"));
    // once only
    assert!(quadra_server::agent::register(&server, &code)
        .unwrap_err()
        .contains("not valid"));
    // only the hashes are kept
    let kept: Vec<String> = Conn::open(&cfg.url)
        .unwrap()
        .query("SELECT \"key_hash\" FROM \"_quadra_agents\"", &[])
        .unwrap()
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect();
    assert_eq!(kept, [quadra_store::agents::hash(&key)]);

    // an event of a connection the agent runs: the job reaches it and comes back
    let stop = Arc::new(AtomicBool::new(false));
    let (h, lines) = agent(port, &key, stop.clone());
    until("the agent", 10, || {
        call(port, &token, "GET", "/v1/agents", None).1["agents"][0]["connected"] == json!(true)
    });
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": "A1" } }] }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    until("the job at the agent", 20, || {
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("erp: insert into SCAMBIO_ORDINI"))
    });
    // what came of it is the outcome of the delivery (here: the agent cannot reach the database
    // yet), with the attempt in the log
    let error = || -> Option<String> {
        Conn::open(&cfg.url)
            .unwrap()
            .query("SELECT \"error\" FROM \"_quadra_outbox\"", &[])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|r| r[0].clone())
    };
    until("the outcome", 20, || error().is_some());
    assert!(!error().unwrap().contains("not connected"), "{:?}", error());
    let (_, list) = call(port, &token, "GET", "/v1/agents", None);
    let a = &list["agents"][0];
    assert_eq!(a["name"], "milano");
    assert_eq!(a["version"], json!(quadra_server::agent::VERSION));
    assert!(a["registered_at"].is_string());

    // revoked: it receives nothing more within 5 s, and stops
    let t = Instant::now();
    let (s, v) = call(port, &token, "POST", "/v1/agents/milano/revoke", None);
    assert_eq!(s, 200, "{v}");
    let r = h.join().unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(r.unwrap_err().contains("revoked"));
    let (_, list) = call(port, &token, "GET", "/v1/agents", None);
    assert_eq!(list["agents"][0]["connected"], json!(false));
    assert!(list["agents"][0]["revoked_at"].is_string());

    // revoked straight in the database (`quadra agent revoke`): within 5 s too
    let (_, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "roma" })),
    );
    let (_, key2) = quadra_server::agent::register(&server, v["code"].as_str().unwrap()).unwrap();
    let (h2, _) = agent(port, &key2, Arc::new(AtomicBool::new(false)));
    until("roma", 10, || {
        call(port, &token, "GET", "/v1/agents", None).1["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "roma" && a["connected"] == json!(true))
    });
    let t = Instant::now();
    assert!(quadra_store::agents::revoke(
        &mut Conn::open(&cfg.url).unwrap(),
        "roma",
        "2026-11-27T12:00:00"
    )
    .unwrap());
    assert!(h2.join().unwrap().is_err());
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_an_admin_manages_agents_and_a_key_is_needed_to_take_jobs() {
    let dir = temp("agents-auth");
    let (cfg, token) = project_at(&dir, "http://127.0.0.1:9", CODE, ERP);
    let (_h, _rt, port) = serve(HostOptions::default(), cfg);
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "Milano!" })),
    );
    assert_eq!(s, 400, "{v}");
    // no key, a wrong key: refused
    let (s, _) = call(port, "", "GET", "/v1/agent/jobs?wait=0", None);
    assert_eq!(s, 401);
    let (s, _) = call(port, "qa_nope", "GET", "/v1/agent/jobs?wait=0", None);
    assert_eq!(s, 401);
    let (s, _) = call(
        port,
        "qa_nope",
        "POST",
        "/v1/agent/results",
        Some(json!({ "results": [] })),
    );
    assert_eq!(s, 401);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_http_service_of_another_network_is_reached_through_the_agent_with_its_secret() {
    // a service only the agent's network reaches (this machine: the server reaches only public
    // addresses), checking the signature made with a secret only the agent has
    let r = crate::deliver::receiver(vec![200]);
    let dir = temp("agents-http");
    let code = "entity Order:\n    number: Text\n\nevent OrderPlaced(order: Order)\n\non Order created as o:\n    emit OrderPlaced(order=o)\n\noutbound OrderPlaced via lan path \"/orders\"\n";
    let settings = format!("\n[vars.LAN_SECRET]\nsecret = true\n\n[connections.lan]\nkind = \"http\"\nurl = \"http://127.0.0.1:{}\"\ninsecure = true\nsigning_secret = {{ var = \"LAN_SECRET\" }}\nlocation = \"agent:negozio\"\n", r.port);
    let (cfg, token) = project_at(&dir, "http://127.0.0.1:9", code, &settings);
    let (_h, _rt, port) = serve(
        HostOptions {
            public_egress_only: true,
            ..Default::default()
        },
        cfg.clone(),
    );
    let (_, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "negozio" })),
    );
    let server = format!("http://127.0.0.1:{port}");
    let (_, key) = quadra_server::agent::register(&server, v["code"].as_str().unwrap()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let l = lines.clone();
    let s2 = stop.clone();
    let h = std::thread::spawn(move || {
        let mut env = std::collections::BTreeMap::new();
        env.insert("LAN_SECRET".to_string(), "agent-only".to_string());
        quadra_server::agent::run(
            quadra_server::agent::Options {
                server,
                key,
                env,
                concurrency: 2,
            },
            move |line| l.lock().unwrap().push(line.to_string()),
            s2,
        )
    });
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": "N1" } }] }),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let state = || -> Vec<String> {
        Conn::open(&cfg.url)
            .unwrap()
            .query("SELECT \"state\" FROM \"_quadra_outbox\"", &[])
            .unwrap()
            .into_iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect()
    };
    until("the delivery through the agent", 30, || {
        state() == ["delivered"]
    });
    let got = r.got.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    let (head, body) = &got[0];
    assert_eq!(head[":path"], "/orders");
    let sig = &head["quadra-signature"];
    let t = sig.split(',').next().unwrap().trim_start_matches("t=");
    assert_eq!(
        sig,
        &format!(
            "t={t},v1={}",
            quadra_server::deliver::sign("agent-only", &format!("{t}.{body}"))
        )
    );
    assert!(head["idempotency-key"].starts_with("quadra-"));
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("lan: POST /orders: 200")),
        "{:?}",
        lines.lock().unwrap()
    );
    stop.store(true, Ordering::Relaxed);
    let _ = h;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pass criterion of the on-premise agent: a database behind a firewall, with no port open
/// to it, exchanges data with an app through the agent: its table pulled by the cursor, rows
/// written to an exchange table, once each. Needs `QUADRA_TEST_PG` (a Postgres the test may
/// create tables in); skipped without it.
#[test]
fn a_database_behind_a_firewall_exchanges_data_with_the_app_through_the_agent() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: skipped");
        return;
    };
    // postgres://user:password@host:port/db
    let rest = pg
        .trim_start_matches("postgres://")
        .trim_start_matches("postgresql://");
    let (cred, place) = rest.split_once('@').unwrap();
    let (user, password) = cred.split_once(':').unwrap();
    let (hostport, db) = place.split_once('/').unwrap();
    let mut c = postgres::Client::connect(&pg, postgres::NoTls).unwrap();
    let t = format!("ordini_{}", std::process::id());
    let x = format!("scambio_{}", std::process::id());
    c.batch_execute(&format!(
        "DROP TABLE IF EXISTS {t}; DROP TABLE IF EXISTS {x};
         CREATE TABLE {t} (id integer PRIMARY KEY, numero text NOT NULL, totale numeric(12,2) NOT NULL);
         INSERT INTO {t} SELECT g, 'N' || g, g * 1.10 FROM generate_series(1, 40) g;
         CREATE TABLE {x} (numero text NOT NULL, totale numeric(12,2) NOT NULL, quadra_key text UNIQUE);"
    ))
    .unwrap();
    let code = format!("entity Order:\n    number: Text\n    total: Decimal\n\nrecord ErpOrder from json:\n    id: Int\n    number: Text at \"numero\"\n    total: Decimal at \"totale\"\n\nrecord ErpBack from json:\n    number: Text at \"numero\"\n    total: Decimal at \"totale\"\n\ninbound OrdersIn via erp as m: ErpOrder:\n    pull \"{t}\" every 1 minutes\n    cursor m.id\n    key f\"order-{{m.id}}\"\n    let o = Order(number=m.number, total=m.total)\n\nevent OrderTaken(order: Order)\n\non Order created as o:\n    emit OrderTaken(order=o)\n\noutbound OrderTaken via erp as e:\n    insert(\"{x}\", ErpBack(number=e.order.number, total=e.order.total))\n");
    let settings = format!("\n[vars.ERP_PASSWORD]\nsecret = true\n\n[connections.erp]\nkind = \"postgres\"\nurl = \"postgres://{hostport}/{db}\"\nuser = \"{user}\"\npassword = {{ var = \"ERP_PASSWORD\" }}\ninsecure = true\nlocation = \"agent:sede\"\n");
    let dir = temp("agents-db");
    let (cfg, token) = project_at(&dir, "http://127.0.0.1:9", &code, &settings);
    // the app reaches public addresses only: the database is the agent's network's
    let (_h, _rt, port) = serve(
        HostOptions {
            public_egress_only: true,
            ..Default::default()
        },
        cfg.clone(),
    );
    let (_, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "sede" })),
    );
    let server = format!("http://127.0.0.1:{port}");
    let (_, key) = quadra_server::agent::register(&server, v["code"].as_str().unwrap()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let pw = password.to_string();
    let h = std::thread::spawn(move || {
        let mut env = std::collections::BTreeMap::new();
        env.insert("ERP_PASSWORD".to_string(), pw);
        quadra_server::agent::run(
            quadra_server::agent::Options {
                server,
                key,
                env,
                concurrency: 4,
            },
            |_| {},
            s2,
        )
    });
    let count = |sql: &str| -> i64 {
        Conn::open(&cfg.url).unwrap().query(sql, &[]).unwrap()[0][0]
            .as_deref()
            .unwrap_or("0")
            .parse()
            .unwrap()
    };
    until("the orders of the table", 60, || {
        count("SELECT COUNT(*) FROM \"order\"") == 40
    });
    let back = |c: &mut postgres::Client| -> i64 {
        c.query_one(&format!("SELECT COUNT(*) FROM {x}"), &[])
            .unwrap()
            .get(0)
    };
    until("the rows written back", 60, || back(&mut c) == 40);
    // exact, once each
    let sum: String = c
        .query_one(&format!("SELECT SUM(totale)::text FROM {x}"), &[])
        .unwrap()
        .get(0);
    assert_eq!(sum, "902.00");
    let distinct: i64 = c
        .query_one(&format!("SELECT COUNT(DISTINCT quadra_key) FROM {x}"), &[])
        .unwrap()
        .get(0);
    assert_eq!(distinct, 40);
    // a row added there comes in at the next pull, from the cursor
    c.batch_execute(&format!("INSERT INTO {t} VALUES (41, 'N41', 5.00)"))
        .unwrap();
    Conn::open(&cfg.url)
        .unwrap()
        .exec("UPDATE \"_quadra_cursors\" SET \"next_at\" = NULL", &[])
        .unwrap();
    until("the new row", 60, || {
        count("SELECT COUNT(*) FROM \"order\"") == 41
    });
    until("its row back", 60, || back(&mut c) == 41);
    stop.store(true, Ordering::Relaxed);
    let _ = h;
    c.batch_execute(&format!("DROP TABLE {t}; DROP TABLE {x};"))
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
