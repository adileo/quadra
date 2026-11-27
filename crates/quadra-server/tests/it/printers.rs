//! Printers (`kind = "printer"`): what `print(…)` gives at the end of an outbound reaches the
//! printer's raw port as it is, a ZPL label or a PDF; a printer that is off keeps the event
//! pending, tried again until it is on; an agent prints on a printer of its own network.

use crate::deliver::{call, serve, temp};
use quadra_runtime::V;
use quadra_sema::Project;
use quadra_server::{Config, HostOptions, Limits, Outside};
use quadra_store::{auth, ops, Conn};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CODE: &str = "from orders import Order\nfrom docs import ParcelLabel, OrderPdf\n\nevent ParcelReady(order: Order)\nevent InvoiceReady(order: Order)\n\non Order created as o:\n    emit ParcelReady(order=o)\n    emit InvoiceReady(order=o)\n\noutbound ParcelReady via zebra as e:\n    backoff 1 seconds .. 1 seconds\n    print(zpl(ParcelLabel(e.order)))\n\noutbound InvoiceReady via office as e:\n    backoff 1 seconds .. 1 seconds\n    print(render(OrderPdf(e.order)))\n";

const DOCS: &str = "from orders import Order\n\nlabel ParcelLabel(o: Order) width 100 height 150 dpi 203:\n    text o.number x 5 y 5 size 14 bold width 90\n    barcode o.number type code128 x 5 y 30 height 20\n\ndocument OrderPdf(o: Order):\n    heading o.number level 1\n    text \"Thank you for your order\"\n";

const ORDERS: &str = "entity Order:\n    number: Text\n";

/// A printer on a port of this machine: the bytes of each job (one per connection).
struct Printer {
    jobs: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn printer_on(l: TcpListener) -> Printer {
    let jobs: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let j = jobs.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { break };
            let mut b = Vec::new();
            let _ = s.read_to_end(&mut b);
            j.lock().unwrap().push(b);
        }
    });
    Printer { jobs }
}

/// A port no one listens on (yet).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// The project in `dir` with `code`, the documents of `DOCS` and the connections of
/// `connections`; its database initialized; a token.
fn project(dir: &Path, code: &str, connections: &str) -> (Config, String) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        format!("[project]\nname = \"d\"\n\n{connections}"),
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    std::fs::write(dir.join("orders.qd"), ORDERS).unwrap();
    std::fs::write(dir.join("docs.qdp"), DOCS).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let checked = quadra_analysis::check(&Project::load(dir).unwrap());
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    ops::init(
        &url,
        checked.model.as_ref().unwrap(),
        &checked.report,
        now(),
    )
    .unwrap();
    let (token, _) = auth::create(
        &mut Conn::open(&url).unwrap(),
        "token",
        1,
        "test",
        false,
        now(),
        None,
    )
    .unwrap();
    let cfg = Config {
        outside: Outside {
            env: BTreeMap::new(),
            deliver: true,
        },
        root: dir.to_path_buf(),
        url,
        port: 0,
        dev_login: false,
        secure_cookies: false,
        cache_mb: 16,
        prewarm_days: None,
        web: None,
        sweep_every: 0,
        editors: vec![],
        watch: false,
        limits: Limits::default(),
        engines: Default::default(),
    };
    (cfg, token)
}

/// What the content of the `print(…)` of the outbound of `event` gives for its event in the
/// outbox: the ZPL (text) or the document rendered, as bytes.
fn expected(cfg: &Config, event: &str) -> Vec<u8> {
    let checked = quadra_analysis::check(&Project::load(&cfg.root).unwrap());
    let model = checked.model.unwrap();
    let o = model.outbounds.iter().find(|o| o.event == event).unwrap();
    let quadra_ir::ExprKind::Block(stmts) = &o.body.as_ref().unwrap().kind else {
        panic!("a block")
    };
    let quadra_ir::StmtKind::Expr(last) = &stmts.last().unwrap().kind else {
        panic!("an expression")
    };
    let quadra_ir::ExprKind::Call { args, .. } = &last.kind else {
        panic!("print(…)")
    };
    let row = &Conn::open(&cfg.url)
        .unwrap()
        .query(
            &format!("SELECT CAST(\"tx\" AS TEXT), \"fields\" FROM \"_quadra_outbox\" WHERE \"event\" = '{event}'"),
            &[],
        )
        .unwrap()[0];
    let tx: u64 = row[0].as_deref().unwrap().parse().unwrap();
    let fields: serde_json::Value = serde_json::from_str(row[1].as_deref().unwrap()).unwrap();
    let v = ops::eval_for_event(
        &cfg.url,
        &model,
        &checked.report,
        now(),
        o,
        &args[0].value,
        &fields,
        tx,
        true,
    )
    .unwrap();
    match v {
        V::Text(t) => t.as_bytes().to_vec(),
        V::Tuple(f) => {
            use base64::Engine as _;
            let content = f
                .iter()
                .find(|(k, _)| k.as_deref() == Some("content"))
                .map(|(_, v)| match v {
                    V::Text(t) => t.to_string(),
                    _ => panic!("the content is text"),
                })
                .unwrap();
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .unwrap()
        }
        other => panic!("{other:?}"),
    }
}

fn until(what: &str, secs: u64, ok: impl Fn() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "waited for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// (state, attempts, error) of the delivery of `event`.
fn delivery(cfg: &Config, event: &str) -> (String, u64, Option<String>) {
    let rows = Conn::open(&cfg.url)
        .unwrap()
        .query(
            &format!("SELECT \"state\", CAST(\"attempts\" AS TEXT), \"error\" FROM \"_quadra_outbox\" WHERE \"event\" = '{event}'"),
            &[],
        )
        .unwrap();
    let Some(r) = rows.into_iter().next() else {
        return (String::new(), 0, None);
    };
    (
        r[0].clone().unwrap_or_default(),
        r[1].as_deref().unwrap_or("0").parse().unwrap(),
        r[2].clone(),
    )
}

fn place(port: u16, token: &str, number: &str) {
    let (s, v) = call(
        port,
        token,
        "POST",
        "/v1/tx",
        Some(
            json!({ "ops": [{ "op": "create", "entity": "Order", "values": { "number": number } }] }),
        ),
    );
    assert_eq!(s, 200, "{v}");
}

#[test]
fn a_label_and_a_pdf_reach_the_printer_as_rendered_and_a_printer_off_is_waited_for() {
    // the label printer is off; the office printer is on
    let zebra_port = free_port();
    let office = TcpListener::bind("127.0.0.1:0").unwrap();
    let office_at = office.local_addr().unwrap().port();
    let office = printer_on(office);
    let dir = temp("printers");
    let (cfg, token) = project(
        &dir,
        CODE,
        &format!("[connections.zebra]\nkind = \"printer\"\nurl = \"tcp://127.0.0.1:{zebra_port}\"\ntimeout = \"2s\"\n\n[connections.office]\nkind = \"printer\"\nurl = \"tcp://127.0.0.1:{office_at}\"\ntimeout = \"5s\"\n"),
    );
    let (_h, _rt, port) = serve(HostOptions::default(), cfg.clone());
    place(port, &token, "A-1001");

    // the PDF, byte for byte what `render(…)` gives
    until("the invoice printed", 30, || {
        delivery(&cfg, "InvoiceReady").0 == "delivered"
    });
    let jobs = office.jobs.lock().unwrap().clone();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].starts_with(b"%PDF-"), "{:?}", &jobs[0][..16]);
    assert_eq!(jobs[0], expected(&cfg, "InvoiceReady"));

    // the label printer off: the event waits, with its attempts logged
    until("an attempt at the label printer", 30, || {
        let (_, attempts, error) = delivery(&cfg, "ParcelReady");
        attempts >= 1 && error.is_some()
    });
    let (state, _, error) = delivery(&cfg, "ParcelReady");
    assert_eq!(state, "pending", "{error:?}");
    assert!(
        error
            .as_deref()
            .unwrap()
            .contains(&format!("127.0.0.1:{zebra_port}")),
        "{error:?}"
    );
    let logged = Conn::open(&cfg.url)
        .unwrap()
        .query(
            "SELECT \"outcome\", \"error\" FROM \"_quadra_exchanges\" WHERE \"direction\" = 'out' AND \"connection\" = 'zebra'",
            &[],
        )
        .unwrap();
    assert!(!logged.is_empty(), "no attempt in the log");
    // on: printed at the next attempt, the ZPL as `zpl(…)` gives it
    let zebra = printer_on(TcpListener::bind(("127.0.0.1", zebra_port)).unwrap());
    until("the label printed", 30, || {
        delivery(&cfg, "ParcelReady").0 == "delivered"
    });
    let jobs = zebra.jobs.lock().unwrap().clone();
    assert_eq!(jobs.len(), 1);
    assert!(
        jobs[0].starts_with(b"^XA"),
        "{}",
        String::from_utf8_lossy(&jobs[0])
    );
    assert!(String::from_utf8_lossy(&jobs[0]).contains("A-1001"));
    assert_eq!(jobs[0], expected(&cfg, "ParcelReady"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_printer_of_the_companys_network_prints_through_the_agent() {
    let zebra = TcpListener::bind("127.0.0.1:0").unwrap();
    let zebra_port = zebra.local_addr().unwrap().port();
    let zebra = printer_on(zebra);
    let office = TcpListener::bind("127.0.0.1:0").unwrap();
    let office_port = office.local_addr().unwrap().port();
    let office = printer_on(office);
    let dir = temp("printers-agent");
    // the label printer through the agent; the office printer without one, from a host that
    // reaches only public addresses: refused for good, and the way said
    let (cfg, token) = project(
        &dir,
        CODE,
        &format!("[connections.zebra]\nkind = \"printer\"\nurl = \"tcp://127.0.0.1:{zebra_port}\"\nlocation = \"agent:backoffice\"\ncopies = 2\n\n[connections.office]\nkind = \"printer\"\nurl = \"tcp://127.0.0.1:{office_port}\"\n"),
    );
    let (_h, _rt, port) = serve(
        HostOptions {
            public_egress_only: true,
            ..Default::default()
        },
        cfg.clone(),
    );
    let (s, v) = call(
        port,
        &token,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "backoffice" })),
    );
    assert_eq!(s, 200, "{v}");
    let server = format!("http://127.0.0.1:{port}");
    let (_, key) = quadra_server::agent::register(&server, v["code"].as_str().unwrap()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let (l, s2) = (lines.clone(), stop.clone());
    let h = std::thread::spawn(move || {
        quadra_server::agent::run(
            quadra_server::agent::Options {
                server,
                key,
                env: BTreeMap::new(),
                concurrency: 2,
            },
            move |line| l.lock().unwrap().push(line.to_string()),
            s2,
        )
    });
    place(port, &token, "B-2002");
    until("the label printed through the agent", 30, || {
        delivery(&cfg, "ParcelReady").0 == "delivered"
    });
    // two copies: two jobs, each the label
    let label = expected(&cfg, "ParcelReady");
    assert!(label.starts_with(b"^XA"));
    assert_eq!(
        zebra.jobs.lock().unwrap().clone(),
        [label.clone(), label.clone()]
    );
    let said = format!("zebra: print {} bytes: done", label.len());
    assert!(
        lines.lock().unwrap().iter().any(|l| l.contains(&said)),
        "{:?}",
        lines.lock().unwrap()
    );
    // the office printer is not reached from the cloud
    until("the office printer refused", 30, || {
        delivery(&cfg, "InvoiceReady").2.is_some()
    });
    let (_, _, error) = delivery(&cfg, "InvoiceReady");
    assert!(
        error.as_deref().unwrap().contains("not a public address")
            && error.as_deref().unwrap().contains("agent"),
        "{error:?}"
    );
    assert!(office.jobs.lock().unwrap().is_empty());
    stop.store(true, Ordering::Relaxed);
    let _ = h;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the compiler ----------------------------------------------------------------------------

/// What the compiler says of `code`, with a printer, a folder of files and an HTTP service.
fn diagnostics(code: &str) -> Vec<String> {
    let dir = temp("printers-check");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("quadra.toml"),
        "[project]\nname = \"d\"\n\n[vars]\nPASS = { secret = true }\n\n[connections.zebra]\nkind = \"printer\"\nurl = \"tcp://192.168.1.50:9100\"\nlocation = \"agent:backoffice\"\n\n[connections.files]\nkind = \"sftp\"\nurl = \"sftp://files.example.com\"\nuser = \"q\"\npassword = { var = \"PASS\" }\nknown_host = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\"\n\n[connections.hooks]\nkind = \"http\"\nurl = \"https://hooks.example.com\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.qd"), code).unwrap();
    std::fs::write(dir.join("orders.qd"), ORDERS).unwrap();
    std::fs::write(dir.join("docs.qdp"), DOCS).unwrap();
    let checked = quadra_analysis::check(&Project::load(&dir).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == quadra_sema::Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn the_compiler_wants_print_for_a_printer_and_a_printer_for_print() {
    let base = "from orders import Order\nfrom docs import ParcelLabel, OrderPdf\n\nrecord Row from json:\n    number: Text\n\nevent ParcelReady(order: Order)\n\n";
    let has = |e: &[String], what: &str| e.iter().any(|e| e.contains(what));
    // fine: a label, a document, text
    for content in [
        "zpl(ParcelLabel(e.order))",
        "render(OrderPdf(e.order))",
        "f\"^XA^FO50,50^FD{e.order.number}^FS^XZ\"",
    ] {
        let e = diagnostics(&format!(
            "{base}outbound ParcelReady via zebra as e:\n    print({content})\n"
        ));
        assert_eq!(e, Vec::<String>::new(), "{content}");
    }
    // a printer takes print only
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via zebra as e:\n    write(\"a.zpl\", zpl(ParcelLabel(e.order)))\n"
    ));
    assert!(has(&e, "`print(content)`"), "{e:?}");
    let e = diagnostics(&format!("{base}outbound ParcelReady via zebra\n"));
    assert!(
        has(&e, "a printer by a block that ends with `print(content)`"),
        "{e:?}"
    );
    // print only to a printer
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via files as e:\n    print(zpl(ParcelLabel(e.order)))\n"
    ));
    assert!(has(&e, "write(path, content)"), "{e:?}");
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via hooks as e:\n    print(zpl(ParcelLabel(e.order)))\n"
    ));
    assert!(has(&e, "post(path, body)"), "{e:?}");
    // what is printed: text or a document
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via zebra as e:\n    print(Row(number=e.order.number))\n"
    ));
    assert!(has(&e, "what is printed is text"), "{e:?}");
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via zebra as e:\n    print(42)\n"
    ));
    assert!(has(&e, "what is printed is text"), "{e:?}");
    let e = diagnostics(&format!(
        "{base}outbound ParcelReady via zebra as e:\n    print(\"a\", \"b\")\n"
    ));
    assert!(has(&e, "`print` takes one value"), "{e:?}");
    // nothing comes in from a printer
    let e = diagnostics(&format!(
        "{base}inbound LabelsIn via zebra as m: Row:\n    pull \"x\" every 5 minutes\n    let o = Order(number=m.number)\n"
    ));
    assert!(
        has(&e, "`zebra` is a printer: nothing comes in from it"),
        "{e:?}"
    );
}
