//! Bulk import: the database it writes is the one the engine writes committing the same
//! records (same values, same formulas, a hash chain `verify` accepts), and data that breaks
//! a rule of the model is refused with nothing written.
//!
//! PostgreSQL runs only when `QUADRA_TEST_PG` holds a url.

use quadra_sema::Project;
use quadra_store::conn::Conn;
use quadra_store::{bulk, ops, Database};
use std::path::{Path, PathBuf};

fn stress() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress")
}

/// The orders of the fixture below, in the engine's words.
const FIXTURE: &str = r#"
from tests.fixtures import world

fixture orders extends world:
    let o1 = Order(number="O-1", customer=rossi, date=2026-03-02)
    let l1 = OrderLine(order=o1, product=hammer, qty=10 pcs)
    OrderLine(order=o1, product=saw, qty=2 pcs, discount=10%, price=18.00 EUR/pcs)
    o1.confirm()
    Shipment(line=l1, qty=4 pcs, date=2026-03-05, tracking="TR1")
    let o2 = Order(number="O-2", customer=bianchi, date=2026-03-03, priority=High)
    let l3 = OrderLine(order=o2, product=hammer, qty=1 pcs)
    o2.confirm()
    Shipment(line=l3, qty=1 pcs, date=2026-03-04, carrier=Air)
    o2.ship()
    let o3 = Order(number="O-3", customer=rossi, date=2026-03-04)
    OrderLine(order=o3, product=saw, qty=3 pcs)
    o3.cancel()
    Order(number="O-4", customer=bianchi, date=2026-03-05, lines=[OrderLine(product=saw, qty=1 pcs)])
"#;

/// A soft rule of the test's own, so that some imported records hold warnings.
const RULES: &str = r#"
extend Order:
    warn priority is not High, "A high-priority order"
"#;

/// The same data as files (fixture `world` of the project, then `orders`). Keys are given in
/// an order that makes references go forward and backward.
fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("User", "id,login,roles\nadmin,admin,Admin\ngiulia,giulia,Salesperson\npaolo,paolo,Salesperson\nmarta,marta,RegionalManager\nluca,luca,Logistics\nvera,vera,Auditor\ncarlo,carlo,Controller\n"),
        ("Region", "id,code,name,manager,logistics,team\nN,N,North,marta,luca,paolo\nS,S,South,marta,,\n"),
        ("Category", "id,code,name,parent\nTH,TH,Hand tools,T\nT,T,Tools,\n"),
        ("Product", "id,sku,name,category,price,cost\nHAM,HAM,Hammer,TH,12.00 EUR/pcs,7.00 EUR\nSAW,SAW,Saw,TH,20.00 EUR/pcs,11.00 EUR\n"),
        ("Tag", "id,name,color\nVIP,VIP,Red\nNEW,NEW,\n"),
        ("Customer", "id,code,name,region,salesperson,tags\nrossi,C1,Rossi Spa,N,giulia,VIP;NEW\nbianchi,C2,Bianchi Srl,S,paolo,NEW\n"),
        ("Order", "id,number,customer,date,state,priority\n1,O-1,rossi,2026-03-02,Confirmed,\n2,O-2,bianchi,2026-03-03,Shipped,High\n3,O-3,rossi,2026-03-04,Cancelled,\n4,O-4,bianchi,2026-03-05,Draft,\n"),
        ("OrderLine", "id,order,product,qty,price,discount\nl1,1,HAM,10 pcs,,\nl2,1,SAW,2 pcs,18.00 EUR/pcs,10%\nl3,2,HAM,1 pcs,,\nl4,3,SAW,3 pcs,,\nl5,4,SAW,1 pcs,,\n"),
        ("Shipment", "line,qty,date,carrier,tracking\nl1,4 pcs,2026-03-05,,TR1\nl3,1 pcs,2026-03-04,Air,\n"),
    ]
}

fn project() -> Project {
    let root = stress();
    let mut sources = Vec::new();
    for dir in [root.clone(), root.join("tests")] {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("qd") {
                sources.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    sources.push((root.join("tests/import_fixture.qd"), FIXTURE.to_string()));
    sources.push((root.join("tests/import_rules.qd"), RULES.to_string()));
    Project::from_sources(&root, sources)
}

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 3, 9).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

/// A fresh database: a SQLite file or a PostgreSQL schema.
fn fresh(target: &str, name: &str) -> String {
    if target == "sqlite" {
        let p = std::env::temp_dir().join(format!("quadra-import-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
        format!("sqlite:{}", p.display())
    } else {
        let schema = format!("qi_{}_{name}", std::process::id());
        Conn::open(target).unwrap().batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")).unwrap();
        format!("{target}#schema={schema}")
    }
}

/// Remove what `fresh` made.
fn cleanup(target: &str, url: &str) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    } else if let Some((_, schema)) = url.split_once("#schema=") {
        let _ = Conn::open(target).and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    }
}

fn write_files(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quadra-import-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (e, text) in files {
        std::fs::write(dir.join(format!("{e}.csv")), text).unwrap();
    }
    dir
}

/// Every projection table as sorted rows, without ids, references and creation metadata
/// (the engine and the import number records in a different order), with whether the
/// record is frozen.
fn tables(url: &str, checked: &quadra_analysis::Checked) -> Vec<(String, Vec<String>)> {
    let model = checked.model.as_ref().unwrap();
    let db = Database::open(url, model, &checked.report).unwrap();
    let mut conn = Conn::open(url).unwrap();
    let mut out = Vec::new();
    for t in &db.schema.tables {
        let cols: Vec<String> = t.columns.iter().filter(|c| c.references.is_none() && !c.name.ends_with("_id") && c.name != "id" && !c.name.starts_with("_created")).map(|c| c.name.clone()).collect();
        if cols.is_empty() {
            continue;
        }
        let shown: Vec<String> = cols.iter().map(|c| if c == "_frozen_tx" { "CASE WHEN \"_frozen_tx\" IS NULL THEN 'live' ELSE 'frozen' END".to_string() } else { format!("CAST(\"{c}\" AS TEXT)") }).collect();
        let mut rows: Vec<String> =
            conn.query(&format!("SELECT {} FROM \"{}\"", shown.join(", "), t.name), &[]).unwrap().into_iter().map(|r| r.iter().map(|v| normalize(v.as_deref())).collect::<Vec<_>>().join(" | ")).collect();
        rows.sort();
        out.push((t.name.clone(), rows));
    }
    out
}

/// The records' metadata as sorted rows, without ids and transactions: entity, frozen or
/// not, warnings, the kinds of their numbers.
fn records(url: &str) -> Vec<String> {
    let mut conn = Conn::open(url).unwrap();
    let mut rows: Vec<String> = conn
        .query("SELECT \"entity\", CASE WHEN \"frozen_tx\" IS NULL THEN 'live' ELSE 'frozen' END, \"warnings\", \"kinds\" FROM \"_quadra_records\"", &[])
        .unwrap()
        .into_iter()
        .map(|r| r.iter().map(|v| v.clone().unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join(" | "))
        .collect();
    rows.sort();
    rows
}

/// PostgreSQL: the foreign keys of the schema, as `table: definition`, sorted.
fn foreign_keys(url: &str) -> Vec<String> {
    let mut conn = Conn::open(url).unwrap();
    let mut rows: Vec<String> = conn
        .query(
            "SELECT CAST(c.conrelid::regclass AS TEXT) || ': ' || pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE n.nspname = current_schema() AND c.contype = 'f'",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    rows.sort();
    rows
}

/// Numbers compare by value (`12.0` and `12`), booleans by truth (`1` and `true`).
fn normalize(v: Option<&str>) -> String {
    match v {
        None => "NULL".into(),
        Some("t" | "true" | "1") => "true".into(),
        Some("f" | "false" | "0") => "false".into(),
        Some(s) => match s.parse::<rust_decimal::Decimal>() {
            Ok(d) => d.normalize().to_string(),
            Err(_) => s.to_string(),
        },
    }
}

fn verify(url: &str, checked: &quadra_analysis::Checked) -> Vec<String> {
    ops::verify(url, checked.model.as_ref().unwrap(), &checked.report, clock()).unwrap().0
}

/// The full-text search index against the text of the rows.
fn search_index(url: &str, checked: &quadra_analysis::Checked) -> Vec<String> {
    let mut db = Database::open(url, checked.model.as_ref().unwrap(), &checked.report).unwrap();
    let schema = db.schema.clone();
    quadra_store::sqlq::search_index_problems(&mut db.conn, &schema, checked.model.as_ref().unwrap()).unwrap()
}

fn import(url: &str, checked: &quadra_analysis::Checked, files: &[(&str, &str)], name: &str, per_commit: Option<usize>) -> Result<bulk::ImportReport, String> {
    let dir = write_files(name, files);
    let r = bulk::import_dir(url, checked.model.as_ref().unwrap(), &checked.report, &dir, clock(), per_commit, &mut |_| {});
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn same_as_the_engine(target: &str) {
    let project = project();
    let checked = quadra_analysis::check(&project);
    let errors: Vec<_> = checked.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error).map(|d| d.message.clone()).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let model = checked.model.as_ref().unwrap();

    let engine = fresh(target, "engine");
    ops::init(&engine, model, &checked.report, clock()).unwrap();
    ops::fixture(&engine, model, &checked.report, "orders", clock()).unwrap();
    assert_eq!(verify(&engine, &checked), Vec::<String>::new());

    // in several commits of a few records each: the chain must hold across them
    let bulk = fresh(target, "bulk");
    ops::init(&bulk, model, &checked.report, clock()).unwrap();
    // (a directory of its own: the SQLite and PostgreSQL tests run at the same time)
    let r = import(&bulk, &checked, &files(), if target == "sqlite" { "same-lite" } else { "same-pg" }, Some(7)).unwrap();
    assert_eq!(r.records, 28);
    assert_eq!(r.commits.len(), 4);
    assert_eq!(verify(&bulk, &checked), Vec::<String>::new());
    // the full-text search: filled by the import (once each table is written), and by commits
    assert_eq!(search_index(&engine, &checked), Vec::<String>::new());
    assert_eq!(search_index(&bulk, &checked), Vec::<String>::new());

    let (a, b) = (tables(&engine, &checked), tables(&bulk, &checked));
    for ((ta, ra), (_, rb)) in a.iter().zip(b.iter()) {
        assert_eq!(ra, rb, "table {ta}: the engine and the import disagree");
    }
    // the records' rows: frozen as `frozen when` says, warnings of the soft rules
    let (ra, rb) = (records(&engine), records(&bulk));
    assert!(rb.iter().any(|r| r.contains("A high-priority order")), "{rb:#?}");
    assert!(rb.iter().any(|r| r.starts_with("Order | frozen")), "{rb:#?}");
    assert_eq!(ra, rb, "_quadra_records: the engine and the import disagree");
    // the foreign keys the import dropped are back
    if target != "sqlite" {
        let fks = foreign_keys(&bulk);
        assert!(fks.iter().any(|k| k.starts_with("order_line: ")), "{fks:#?}");
        assert_eq!(foreign_keys(&engine), fks);
    }
    // rollups of rollups, from the materialized columns
    let mut c = Conn::open(&bulk).unwrap();
    let rev = c.query("SELECT CAST(\"revenue\" AS TEXT), CAST(\"to_ship\" AS TEXT) FROM \"region\" WHERE \"code\" = 'N'", &[]).unwrap();
    assert_eq!(normalize(rev[0][0].as_deref()), "152.4");
    assert_eq!(normalize(rev[0][1].as_deref()), "8");

    // the engine works on the imported data: a commit through the usual path
    let mut db = Database::open(&bulk, model, &checked.report).unwrap();
    let mut interp = ops::loaded(&mut db, model, clock()).unwrap();
    interp.materialize = quadra_store::materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(quadra_store::Sink { db, model }));
    interp.system += 1;
    interp
        .transaction(|i| {
            let line = i.ids("OrderLine", None)[0];
            i.set(&quadra_runtime::V::Ref { ent: "OrderLine".into(), id: line }, "qty", quadra_runtime::V::Qty { v: 11.into(), unit: "pcs".into() }, None)
        })
        .unwrap();
    assert_eq!(verify(&bulk, &checked), Vec::<String>::new());
    assert_eq!(search_index(&bulk, &checked), Vec::<String>::new());
    cleanup(target, &engine);
    cleanup(target, &bulk);
}

#[test]
fn the_import_writes_what_the_engine_writes_on_sqlite() {
    same_as_the_engine("sqlite");
}

#[test]
fn the_import_writes_what_the_engine_writes_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    same_as_the_engine(&pg);
}

fn with(file: &str, text: &'static str) -> Vec<(&'static str, &'static str)> {
    files().into_iter().map(|(e, t)| if e == file { (e, text) } else { (e, t) }).collect()
}

/// Data that breaks the model is refused, with a message that says where; nothing is
/// written.
#[test]
fn data_that_breaks_the_model_is_refused() {
    let project = project();
    let checked = quadra_analysis::check(&project);
    let model = checked.model.as_ref().unwrap();
    let url = fresh("sqlite", "refused");
    ops::init(&url, model, &checked.report, clock()).unwrap();
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (with("Shipment", "line,qty,date\nl1,40 pcs,2026-03-05\n"), "Shipped more than ordered"),
        (with("Shipment", "line,qty,date\nl1,1 pcs,2026-03-01\n"), "Shipped before it was ordered"),
        (with("Shipment", "line,qty,date\nl1,0 pcs,2026-03-05\n"), "Shipment.qty = 0 pcs: does not satisfy its `where`"),
        (with("Tag", "id,name\nVIP,VIP\nNEW,VIP\n"), "Tag.name must be unique: VIP is given twice"),
        (with("Order", "id,number,customer,date,state\n1,O-1,rossi,,Confirmed\n2,O-2,bianchi,2026-03-03,Shipped\n3,O-3,rossi,2026-03-04,Cancelled\n4,O-4,bianchi,2026-03-05,Draft\n"), "Order.date is required"),
        (with("Shipment", "line,qty,date\nl9,1 pcs,2026-03-05\n"), "no OrderLine has the key `l9`"),
        (with("Shipment", "line,qty,date,shipped\nl1,1 pcs,2026-03-05,1\n"), "column `shipped`"),
        (with("Order", "id,number,customer,date,state\n1,O-1,rossi,2026-03-02,Sent\n"), "`Sent` is not one of Draft, Confirmed, Shipped, Cancelled"),
        (with("Product", "id,sku,name,category,price,cost\nHAM,HAM,Hammer,TH,12.00 USD/pcs,7.00 EUR\n"), "expected a price in EUR/pcs"),
    ];
    let before = verify(&url, &checked);
    assert!(before.is_empty());
    for (i, (files, expected)) in cases.into_iter().enumerate() {
        match import(&url, &checked, &files, &format!("bad{i}"), None) {
            Ok(_) => panic!("case {i}: imported, expected an error containing {expected:?}"),
            Err(e) => assert!(e.contains(expected), "case {i}: {e:?} does not contain {expected:?}"),
        }
    }
    let status = Database::open(&url, model, &checked.report).unwrap().status().unwrap();
    assert_eq!(status.commits, 1, "only the seeds are committed");
    cleanup("sqlite", &url);
}
