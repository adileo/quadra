//! Incremental deploys against the reference: for every kind of change, the same data is
//! deployed incrementally on one database and by rebuilding every table (`ops::deploy_full`,
//! the behaviour before the catalog) on another. Both must hold the same projection rows,
//! the same history, the same signed commits, and pass `verify`; a change the data breaks
//! must fail the same way on both.
//!
//! PostgreSQL runs only when `QUADRA_TEST_PG` holds a url.

use quadra_sema::Project;
use quadra_store::conn::{Conn, Dialect};
use quadra_store::ops;
use std::collections::BTreeMap;
use std::path::Path;

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 12, 1).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

/// The model the changes start from, with data: customers, their orders, tags.
const V1: &str = r#"
entity Customer:
    code: Code unique
    name: Text
    city: Text?
    credit: Int default 100
    vip: Bool default false
    tags: {Tag}
    orders: [Order]
    total = orders.sum(o => o.amount)
    order_count = orders.count()
    noted = orders.count(o => o.note != none)
    check credit >= 0, "Credit cannot be negative"

entity Order:
    number: Code unique
    customer: Customer
    amount: Int
    qty: Int default 1
    note: Text?
    state: enum(Draft, Done, Cancelled) default Draft
    lines: [Line]
    weight = lines.sum(l => l.units)
    frozen when state is Cancelled
    check note != "bad", "A bad note"

entity Line:
    order: Order parent
    units: Int
    label: Text default "line"

entity Tag:
    name: Code unique
    customers: {Customer} via tags
    notes = Order.count(o => o.note != none)
"#;

fn seed() -> String {
    let mut s = String::from("seed:\n    let red = Tag(name=\"red\")\n    let blue = Tag(name=\"blue\")\n");
    for c in 1..=23 {
        let tags = match c % 3 {
            0 => "{red}",
            1 => "{red, blue}",
            _ => "{}",
        };
        let city = if c % 4 == 0 { "none".to_string() } else { format!("\"City {}\"", c % 5) };
        s.push_str(&format!("    let c{c} = Customer(code=\"C{c:02}\", name=\"Customer {c}\", city={city}, credit={}, tags={tags})\n", c * 10));
        for o in 1..=(c % 4 + 1) {
            let state = match (c + o) % 5 {
                0 => "Done",
                _ => "Draft",
            };
            s.push_str(&format!("    let o{c}_{o} = Order(number=\"O{c:02}{o}\", customer=c{c}, amount={}, state={state}, lines=[Line(units={o}), Line(units={})])\n", c * o, c + o));
        }
    }
    s.push_str("    o5_1.note = \"rush\"\n");
    s
}

struct Case {
    name: &'static str,
    /// the model after the change (V1 with replacements)
    edits: Vec<(&'static str, &'static str)>,
    /// files added to the project (migrations)
    files: Vec<(&'static str, &'static str)>,
    /// None: the deploy succeeds; Some(part): it fails with a message containing it
    fails: Option<&'static str>,
}

fn cases() -> Vec<Case> {
    vec![
        Case { name: "noop", edits: vec![], files: vec![], fails: None },
        Case { name: "add_field", edits: vec![("    vip: Bool default false\n", "    vip: Bool default false\n    segment: Text?\n")], files: vec![], fails: None },
        Case { name: "add_default", edits: vec![("    note: Text?\n", "    note: Text?\n    channel: enum(Web, Phone) default Web\n    code2: Text default f\"{number}-{qty}\"\n")], files: vec![], fails: None },
        Case { name: "default_on_existing_field", edits: vec![("    note: Text?\n", "    note: Text? default \"plain\"\n")], files: vec![], fails: None },
        Case { name: "default_breaks_a_check", edits: vec![("    note: Text?\n", "    note: Text? default \"bad\"\n")], files: vec![], fails: Some("A bad note") },
        Case { name: "add_ref", edits: vec![("    note: Text?\n", "    note: Text?\n    referrer: Customer?\n    favourite: Tag?\n")], files: vec![], fails: None },
        Case { name: "add_set", edits: vec![("    vip: Bool default false\n", "    vip: Bool default false\n    likes: {Tag}\n")], files: vec![], fails: None },
        Case { name: "remove_field", edits: vec![("    city: Text?\n", ""), ("    label: Text default \"line\"\n", "")], files: vec![], fails: None },
        Case { name: "remove_set", edits: vec![("    tags: {Tag}\n", ""), ("    customers: {Customer} via tags\n", "")], files: vec![], fails: None },
        Case {
            name: "rename_field",
            edits: vec![("    name: Text\n    city", "    title: Text\n    city"), ("    tags: {Tag}\n", "    labels: {Tag}\n"), ("via tags", "via labels")],
            files: vec![("migrations/m1.qd", "migration rename_name:\n    rename Customer.name -> Customer.title\n    rename Customer.tags -> Customer.labels\n")],
            fails: None,
        },
        Case {
            name: "rename_ref",
            edits: vec![("    customer: Customer\n    amount", "    buyer: Customer\n    amount"), ("    orders: [Order]\n", "    orders: [Order] via buyer\n")],
            files: vec![("migrations/m1.qd", "migration rename_buyer:\n    rename Order.customer -> Order.buyer\n")],
            fails: None,
        },
        Case { name: "removed_and_added_without_rename", edits: vec![("    name: Text\n    city", "    title: Text\n    city")], files: vec![], fails: Some("Customer.title is required") },
        Case { name: "type_change", edits: vec![("    amount: Int\n", "    amount: Decimal\n")], files: vec![], fails: None },
        Case { name: "type_change_of_ref", edits: vec![("    units: Int\n", "    units: Decimal\n")], files: vec![], fails: None },
        Case { name: "new_unique", edits: vec![("    vip: Bool default false\n", "    vip: Bool default false\n    unique (name, credit)\n")], files: vec![], fails: None },
        Case { name: "new_unique_broken", edits: vec![("    label: Text default \"line\"\n", "    label: Text default \"line\"\n    unique (label)\n")], files: vec![], fails: Some("must be unique") },
        Case { name: "removed_unique", edits: vec![("    number: Code unique\n", "    number: Code\n")], files: vec![], fails: None },
        Case { name: "new_check", edits: vec![("    amount: Int\n", "    amount: Int\n    check amount < 1000, \"too much\"\n")], files: vec![], fails: None },
        Case { name: "new_check_broken", edits: vec![("    amount: Int\n", "    amount: Int\n    check amount < 20, \"Amounts stay under 20\"\n")], files: vec![], fails: Some("Amounts stay under 20") },
        Case { name: "new_required", edits: vec![("    city: Text?\n", "    city: Text\n")], files: vec![], fails: Some("Customer.city is required") },
        Case { name: "new_required_when", edits: vec![("    city: Text?\n", "    city: Text? required when credit > 1000\n")], files: vec![], fails: None },
        Case { name: "new_required_when_broken", edits: vec![("    city: Text?\n", "    city: Text? required when credit > 100\n")], files: vec![], fails: Some("Customer.city is required") },
        Case {
            name: "new_required_and_check_broken",
            edits: vec![("    city: Text?\n", "    city: Text? required when credit > 100\n    check credit < 150, \"credit too high\"\n")],
            files: vec![],
            fails: Some("credit too high"),
        },
        Case { name: "new_where", edits: vec![("    qty: Int default 1\n", "    qty: Int where > 0 default 1\n")], files: vec![], fails: None },
        Case { name: "new_where_on_a_ref", edits: vec![("    customer: Customer\n", "    customer: Customer where c => c.credit >= 10 and c.name != \"\"\n")], files: vec![], fails: None },
        Case { name: "new_where_range", edits: vec![("    credit: Int default 100\n", "    credit: Int where 0..1000 default 100\n")], files: vec![], fails: None },
        Case { name: "new_where_broken", edits: vec![("    amount: Int\n", "    amount: Int where a => a < 20\n")], files: vec![], fails: Some("does not satisfy its `where`") },
        Case { name: "new_where_and_check_broken", edits: vec![("    amount: Int\n", "    amount: Int where a => a < 40\n    check amount < 30, \"amount over 30\"\n")], files: vec![], fails: Some("amount over 30") },
        Case { name: "new_unique_field", edits: vec![("    name: Text\n    city", "    name: Text unique\n    city")], files: vec![], fails: None },
        Case { name: "new_unique_of_two", edits: vec![("    vip: Bool default false\n", "    vip: Bool default false\n    unique (code, city)\n")], files: vec![], fails: None },
        Case { name: "new_unique_field_broken", edits: vec![("    units: Int\n", "    units: Int unique\n")], files: vec![], fails: Some("must be unique") },
        Case {
            name: "new_uniques_broken",
            edits: vec![("    label: Text default \"line\"\n", "    label: Text default \"line\"\n    unique (label, units)\n    unique (order, units)\n")],
            files: vec![],
            fails: Some("already exists"),
        },
        Case { name: "new_warn", edits: vec![("    amount: Int\n", "    amount: Int\n    warn amount < 30, \"big order\"\n")], files: vec![], fails: None },
        Case { name: "formula_change", edits: vec![("    total = orders.sum(o => o.amount)\n", "    total = orders.sum(o => o.amount * 2)\n")], files: vec![], fails: None },
        Case { name: "new_formula", edits: vec![("    order_count = orders.count()\n", "    order_count = orders.count()\n    big = orders.count(o => o.amount > 20)\n")], files: vec![], fails: None },
        Case { name: "new_entity", edits: vec![], files: vec![("more.qd", "from main import Customer\n\nentity Visit:\n    customer: Customer\n    day: Date\n")], fails: None },
        Case {
            name: "removed_entity",
            edits: vec![("    tags: {Tag}\n", ""), ("entity Tag:\n    name: Code unique\n    customers: {Customer} via tags\n    notes = Order.count(o => o.note != none)\n", "")],
            files: vec![],
            fails: None,
        },
        Case { name: "frozen_change", edits: vec![("    frozen when state is Cancelled\n", "    frozen when state is Done\n")], files: vec![], fails: None },
        Case {
            name: "migration_with_code",
            edits: vec![("    vip: Bool default false\n", "    vip: Bool default false\n    level: Int default 0\n")],
            files: vec![("migrations/m1.qd", "from main import Customer\n\nmigration levels:\n    for c in Customer.all():\n        if c.credit > 100:\n            c.level = 2\n            c.vip = true\n")],
            fails: None,
        },
    ]
}

/// A project on disk: V1 with the case's edits (and its seed only when `seed`).
fn project(dir: &Path, case: &Case, seed_it: bool) -> Project {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("migrations")).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"deploy\"\n").unwrap();
    let mut src = V1.to_string();
    for (from, to) in &case.edits {
        assert!(src.contains(from), "{}: `{from}` not in the model", case.name);
        src = src.replacen(from, to, 1);
    }
    if seed_it {
        src.push_str(&seed());
    }
    std::fs::write(dir.join("main.qd"), src).unwrap();
    for (path, text) in &case.files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    Project::load(dir).unwrap()
}

fn checked(p: &Project, what: &str) -> quadra_analysis::Checked {
    let c = quadra_analysis::check(p);
    let errors: Vec<String> = c.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error).map(|d| d.message.clone()).collect();
    assert!(c.model.is_some() && errors.is_empty(), "{what} does not check: {errors:?}");
    c
}

/// Every table as rows of (column → text), sorted: the projections and the truth.
fn dump(url: &str) -> BTreeMap<String, Vec<BTreeMap<String, Option<String>>>> {
    let mut c = Conn::open(url).unwrap();
    let tables: Vec<String> = match c.dialect() {
        Dialect::Sqlite => c.query("SELECT name FROM sqlite_master WHERE type = 'table'", &[]).unwrap(),
        Dialect::Postgres => c.query("SELECT CAST(table_name AS TEXT) FROM information_schema.tables WHERE table_schema = current_schema()", &[]).unwrap(),
    }
    .into_iter()
    .filter_map(|r| r[0].clone())
    .filter(|t| !matches!(t.as_str(), "_quadra_meta" | "_quadra_tokens" | "_quadra_idempotency"))
    // (the full-text search's FTS5 tables on SQLite: an index, compared by `search_index`)
    .filter(|t| !t.starts_with("qx_"))
    .collect();
    let mut out = BTreeMap::new();
    for t in tables {
        let cols: Vec<String> = match c.dialect() {
            Dialect::Sqlite => c.query(&format!("SELECT name FROM pragma_table_info('{t}')"), &[]).unwrap(),
            Dialect::Postgres => {
                c.query("SELECT CAST(column_name AS TEXT) FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = ?text", &[quadra_store::conn::Sql::Text(t.clone())]).unwrap()
            }
        }
        .into_iter()
        .filter_map(|r| r[0].clone())
        // (`version_tx`: compared apart, see `versions`)
        .filter(|c| !(t == "_quadra_records" && (c == "last_tx" || c == "version_tx")))
        .collect();
        let list: Vec<String> = cols.iter().map(|c| format!("CAST(\"{c}\" AS TEXT)")).collect();
        let mut rows: Vec<BTreeMap<String, Option<String>>> = c.query(&format!("SELECT {} FROM \"{t}\"", list.join(", ")), &[]).unwrap().into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect();
        rows.sort_by_key(|r| format!("{r:?}"));
        out.insert(t, rows);
    }
    out
}

/// `_quadra_records.version_tx` of each record.
/// The full-text search against the text of the rows (`sqlq::search_index_problems`).
fn search_index(url: &str, m: &quadra_ir::Model, r: &quadra_types::Report) -> Vec<String> {
    let mut db = quadra_store::Database::open(url, m, r).unwrap();
    let schema = db.schema.clone();
    quadra_store::sqlq::search_index_problems(&mut db.conn, &schema, m).unwrap()
}

fn versions(url: &str) -> BTreeMap<String, i64> {
    let mut c = Conn::open(url).unwrap();
    c.query("SELECT CAST(\"id\" AS TEXT), CAST(\"version_tx\" AS TEXT) FROM \"_quadra_records\"", &[])
        .unwrap()
        .into_iter()
        .map(|r| (r[0].clone().unwrap_or_default(), r[1].as_deref().and_then(|v| v.parse().ok()).unwrap_or(0)))
        .collect()
}

fn fresh(target: &str, name: &str) -> String {
    if target == "sqlite" {
        let file = std::env::temp_dir().join(format!("quadra-deploy-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
        }
        format!("sqlite:{}", file.display())
    } else {
        let schema = format!("deploy_{}_{name}", std::process::id());
        let mut c = Conn::open(target).unwrap();
        c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")).unwrap();
        format!("{target}#schema={schema}")
    }
}

fn wipe(url: &str) {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    } else if let Some((base, schema)) = url.split_once("#schema=") {
        if let Ok(mut c) = Conn::open(base) {
            let _ = c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"));
        }
    }
}

fn run_cases(target: &str) {
    quadra_store::deploy::BATCH.store(7, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("quadra-deploy-{}-{}", std::process::id(), if target == "sqlite" { "lite" } else { "pg" }));
    let mut failures = Vec::new();
    let only = std::env::var("QX_CASE").ok();
    for case in cases().into_iter().filter(|c| only.as_deref().map_or(true, |o| o == c.name)) {
        let dir1 = root.join(format!("{}-v1", case.name));
        let dir2 = root.join(format!("{}-v2", case.name));
        let base = Case { name: "v1", edits: vec![], files: vec![], fails: None };
        let p1 = project(&dir1, &base, true);
        let c1 = checked(&p1, "v1");
        let p2 = project(&dir2, &case, false);
        let c2 = checked(&p2, case.name);
        let (m1, m2) = (c1.model.as_ref().unwrap(), c2.model.as_ref().unwrap());
        let inc = fresh(target, &format!("{}_inc", case.name));
        let full = fresh(target, &format!("{}_full", case.name));
        for url in [&inc, &full] {
            ops::init(url, m1, &c1.report, clock()).unwrap();
        }
        let a = ops::deploy(&inc, m2, &c2.report, &dir2, clock(), false, "test deploy");
        let b = ops::deploy_full(&full, m2, &c2.report, &dir2, clock(), false, "test deploy");
        match (&case.fails, &a, &b) {
            (Some(part), Err(ea), Err(eb)) => {
                eprintln!("{} on {target}: refused: {}", case.name, ea.lines().next().unwrap_or(""));
                if !ea.contains(part) || ea != eb {
                    failures.push(format!("{}: the errors differ:\n  incremental: {ea}\n  full: {eb}", case.name));
                }
            }
            (None, Ok(ra), Ok(_)) => {
                eprintln!("{} on {target}: {:?}, {} new defaults, indexes +{:?} -{:?}", case.name, ra.steps, ra.new_defaults, ra.indexes_created, ra.indexes_dropped);
                if !ra.incremental || ra.rebuilt_because.is_some() {
                    failures.push(format!("{}: not incremental ({:?})", case.name, ra.rebuilt_because));
                }
                // new rules on data the deploy does not change are checked in SQL
                let in_sql = ["new_check", "new_required_when", "new_where", "new_where_range", "new_unique", "new_unique_of_two"];
                if in_sql.contains(&case.name) && !ra.steps.iter().any(|s| s.contains("in SQL")) {
                    failures.push(format!("{}: not checked in SQL: {:?}", case.name, ra.steps));
                }
                // a record read from its projection row (`version_tx` set) reads what the
                // rebuild's does; 0 (read from the history) is always right
                let (va, vb) = (versions(&inc), versions(&full));
                for (id, v) in &va {
                    if *v != 0 && vb.get(id) != Some(v) {
                        failures.push(format!("{}: record {id} has version_tx {v}, the rebuild {:?}", case.name, vb.get(id)));
                    }
                }
                let (da, db) = (dump(&inc), dump(&full));
                if da != db {
                    let mut diff = Vec::new();
                    for (t, rows) in &da {
                        match db.get(t) {
                            None => diff.push(format!("table {t} only in the incremental deploy")),
                            Some(other) if other != rows => {
                                let first = rows.iter().zip(other.iter()).find(|(x, y)| x != y);
                                diff.push(format!("table {t}: {} rows vs {}; first difference:\n    {:?}", rows.len(), other.len(), first));
                            }
                            _ => {}
                        }
                    }
                    for t in db.keys().filter(|t| !da.contains_key(*t)) {
                        diff.push(format!("table {t} only in the full deploy"));
                    }
                    failures.push(format!("{}: different databases:\n  {}", case.name, diff.join("\n  ")));
                }
                for url in [&inc, &full] {
                    let problems = ops::verify(url, m2, &c2.report, clock()).unwrap().0;
                    if !problems.is_empty() {
                        failures.push(format!("{}: verify on {url}: {problems:?}", case.name));
                    }
                    // the full-text search follows renamed, removed and added fields
                    let problems = search_index(url, m2, &c2.report);
                    if !problems.is_empty() {
                        failures.push(format!("{}: full-text search on {url}: {}", case.name, problems.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
                    }
                }
                // a second deploy of the same code changes nothing but the log
                let again = ops::deploy(&inc, m2, &c2.report, &dir2, clock(), false, "again").unwrap();
                if !again.steps.is_empty() || again.new_defaults > 0 {
                    failures.push(format!("{}: a second deploy did something: {:?}", case.name, again.steps));
                }
                let problems = ops::verify(&inc, m2, &c2.report, clock()).unwrap().0;
                if !problems.is_empty() {
                    failures.push(format!("{}: verify after a second deploy: {problems:?}", case.name));
                }
            }
            _ => failures.push(format!("{}: unexpected outcome:\n  incremental: {:?}\n  full: {:?}", case.name, a.as_ref().map(|r| &r.steps), b.as_ref().map(|r| &r.steps))),
        }
        if case.fails.is_some() {
            // nothing written: the database is still at its first commit
            let mut db = quadra_store::Database::open(&inc, m1, &c1.report).unwrap();
            if db.status().unwrap().commits != 1 {
                failures.push(format!("{}: a failed deploy wrote something", case.name));
            }
        }
        wipe(&inc);
        wipe(&full);
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn incremental_deploys_match_the_rebuild_on_sqlite() {
    run_cases("sqlite");
}

#[test]
fn incremental_deploys_match_the_rebuild_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped");
        return;
    };
    run_cases(&pg);
}

/// A database created before the catalog existed: opened, upgraded (system indexes), its first
/// deploy rebuilds the tables and writes the catalog, the next ones are incremental.
#[test]
fn a_database_without_catalog_is_upgraded() {
    let dir = std::env::temp_dir().join(format!("quadra-deploy-{}-old", std::process::id()));
    let base = Case { name: "v1", edits: vec![], files: vec![], fails: None };
    let p1 = project(&dir, &base, true);
    let c1 = checked(&p1, "v1");
    let m1 = c1.model.as_ref().unwrap();
    let url = fresh("sqlite", "old");
    ops::init(&url, m1, &c1.report, clock()).unwrap();
    // as an older version left it: no catalog, no system version, no indexes
    let mut c = Conn::open(&url).unwrap();
    c.batch("DELETE FROM _quadra_meta WHERE key IN ('catalog', 'system')").unwrap();
    for (name, _, _) in quadra_store::db::SYSTEM_INDEXES {
        c.batch(&format!("DROP INDEX \"{name}\"")).unwrap();
    }
    let qx: Vec<String> = c.query("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'qx_%'", &[]).unwrap().into_iter().filter_map(|r| r[0].clone()).collect();
    for n in qx {
        c.batch(&format!("DROP INDEX \"{n}\"")).unwrap();
    }
    drop(c);
    let before = dump(&url);
    let mut db = quadra_store::Database::open(&url, m1, &c1.report).unwrap();
    let n: usize = db.conn.query("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'qx\\_\\_quadra%' ESCAPE '\\'", &[]).unwrap().len();
    assert_eq!(n, quadra_store::db::SYSTEM_INDEXES.len(), "the system indexes are added on open");
    drop(db);
    assert_eq!(dump(&url), before, "opening changes no data");
    let first = ops::deploy(&url, m1, &c1.report, &dir, clock(), false, "first").unwrap();
    assert!(!first.incremental && first.rebuilt_because.is_some());
    let second = ops::deploy(&url, m1, &c1.report, &dir, clock(), false, "second").unwrap();
    assert!(second.incremental, "{:?}", second.rebuilt_because);
    assert!(ops::verify(&url, m1, &c1.report, clock()).unwrap().0.is_empty());
    let mut db = quadra_store::Database::open(&url, m1, &c1.report).unwrap();
    let names: Vec<String> = db.existing_indexes().unwrap().into_iter().map(|(n, _, _)| n).collect();
    // (SQLite: the full-text search is an FTS5 table, not an index)
    for i in db.schema.indexes.clone().iter().filter(|i| !i.fts) {
        assert!(names.contains(&i.name), "index {} missing: {names:?}", i.name);
    }
    drop(db);
    assert_eq!(search_index(&url, m1, &c1.report), Vec::<String>::new());
    wipe(&url);
    let _ = std::fs::remove_dir_all(&dir);
}

/// PostgreSQL: an index left invalid (a `CREATE INDEX CONCURRENTLY` interrupted after the
/// deploy's commit) is dropped and built again by the next `ensure_indexes`.
#[test]
fn an_invalid_index_is_built_again_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("quadra-deploy-{}-invalid", std::process::id()));
    let base = Case { name: "v1", edits: vec![], files: vec![], fails: None };
    let p1 = project(&dir, &base, true);
    let c1 = checked(&p1, "v1");
    let m1 = c1.model.as_ref().unwrap();
    let url = fresh(&pg, "invalid");
    ops::init(&url, m1, &c1.report, clock()).unwrap();
    let mut c = Conn::open(&url).unwrap();
    // what an interrupted concurrent build leaves (needs a superuser: skipped otherwise)
    if c.batch("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'qx_order_customer_id'::regclass").is_err() {
        eprintln!("not a superuser: skipped");
        wipe(&url);
        return;
    }
    drop(c);
    let mut db = quadra_store::Database::open(&url, m1, &c1.report).unwrap();
    assert!(db.existing_indexes().unwrap().iter().any(|(n, _, valid)| n == "qx_order_customer_id" && !valid));
    let (created, dropped) = db.ensure_indexes(None).unwrap();
    assert!(created.contains(&"qx_order_customer_id".to_string()) && dropped.contains(&"qx_order_customer_id".to_string()), "{created:?} {dropped:?}");
    assert!(db.existing_indexes().unwrap().iter().all(|(_, _, valid)| *valid));
    drop(db);
    wipe(&url);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A model whose entities inherit fields from a trait of a trait (`Note is Numbered`,
/// `Numbered is Doc`): the inherited fields have their columns (they used to be left out of
/// the implementor's table, and its records were read from their history).
const TRAITS: &str = r#"
trait Doc:
    n: Int
    title: Text default "doc"
    unique (n)

trait Numbered is Doc:
    code: Code

entity Note is Numbered:
    body: Text
    labels: {Label}

entity Memo is Doc:
    x: Int

entity Label:
    name: Code unique
"#;

fn traits_seed() -> String {
    let mut s = String::from("seed:\n    let a = Label(name=\"a\")\n    let b = Label(name=\"b\")\n");
    for i in 1..=12 {
        let labels = match i % 3 {
            0 => "{a}",
            1 => "{a, b}",
            _ => "{}",
        };
        let title = if i % 4 == 0 { String::new() } else { format!(", title=\"T{i}\"") };
        s.push_str(&format!("    Note(n={i}, code=\"N{i:02}\", body=\"note {i}\", labels={labels}{title})\n"));
        s.push_str(&format!("    Memo(n={}, x={i})\n", 100 + i));
    }
    s
}

fn traits_project(dir: &Path, edits: &[(&str, &str)], seed_it: bool) -> Project {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"traits\"\n").unwrap();
    let mut src = TRAITS.to_string();
    for (from, to) in edits {
        assert!(src.contains(from), "`{from}` not in the model");
        src = src.replacen(from, to, 1);
    }
    if seed_it {
        src.push_str(&traits_seed());
    }
    std::fs::write(dir.join("main.qd"), src).unwrap();
    Project::load(dir).unwrap()
}

/// What a database deployed before the fix looked like: the `note` table without the columns
/// of `Doc` (and its catalog saying so), its records read from their history.
fn as_before_the_fix(url: &str) {
    let mut c = Conn::open(url).unwrap();
    let cat: String = c.query("SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = 'catalog'", &[]).unwrap()[0][0].clone().unwrap();
    let mut cat: serde_json::Value = serde_json::from_str(&cat).unwrap();
    let gone = ["n", "title"];
    for t in cat["tables"].as_array_mut().unwrap() {
        if t["name"] != "note" {
            continue;
        }
        t["fields"].as_array_mut().unwrap().retain(|f| !gone.contains(&f["field"].as_str().unwrap()));
        t["columns"].as_array_mut().unwrap().retain(|col| !gone.contains(&col["name"].as_str().unwrap()));
        t["uniques"] = serde_json::json!([]);
    }
    cat["indexes"].as_array_mut().unwrap().retain(|i| !(i["table"] == "note" && i["columns"].as_array().unwrap().iter().any(|x| gone.contains(&x.as_str().unwrap()))));
    let text = serde_json::to_string(&cat).unwrap();
    c.exec("UPDATE \"_quadra_meta\" SET \"value\" = ?text WHERE \"key\" = 'catalog'", &[quadra_store::conn::Sql::Text(text)]).unwrap();
    match c.dialect() {
        Dialect::Sqlite => {
            c.batch("PRAGMA foreign_keys = OFF").unwrap();
            let cols: Vec<String> = c.query("SELECT name FROM pragma_table_info('note')", &[]).unwrap().into_iter().filter_map(|r| r[0].clone()).filter(|n| !gone.contains(&n.as_str())).collect();
            let list: Vec<String> = cols.iter().map(|x| format!("\"{x}\"")).collect();
            c.batch(&format!("CREATE TABLE note_old AS SELECT {} FROM note", list.join(", "))).unwrap();
            c.batch("DROP TABLE note").unwrap();
            c.batch("ALTER TABLE note_old RENAME TO note").unwrap();
            c.batch("PRAGMA foreign_keys = ON").unwrap();
        }
        Dialect::Postgres => {
            c.batch("ALTER TABLE note DROP COLUMN n CASCADE, DROP COLUMN title CASCADE").unwrap();
        }
    }
    c.batch("UPDATE \"_quadra_records\" SET \"version_tx\" = 0 WHERE \"entity\" = 'Note'").unwrap();
    c.batch("UPDATE \"_quadra_meta\" SET \"value\" = 'before' WHERE \"key\" = 'schema_fingerprint'").unwrap();
}

fn traits_cases(target: &str) {
    quadra_store::deploy::BATCH.store(5, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("quadra-deploy-{}-traits-{}", std::process::id(), if target == "sqlite" { "lite" } else { "pg" }));
    let mut failures = Vec::new();
    // (name, edits of the model deployed, the database is as before the fix)
    let cases: Vec<(&str, Vec<(&str, &str)>, bool)> = vec![
        ("upgrade", vec![], true),
        ("noop", vec![], false),
        ("add_field_to_the_root_trait", vec![("    title: Text default \"doc\"\n", "    title: Text default \"doc\"\n    note: Text?\n    level: Int default 3\n")], false),
        ("add_field_and_upgrade", vec![("    title: Text default \"doc\"\n", "    title: Text default \"doc\"\n    level: Int default 3\n")], true),
    ];
    for (name, edits, before) in cases {
        let p1 = traits_project(&root.join(format!("{name}-v1")), &[], true);
        let c1 = checked(&p1, "traits v1");
        let dir2 = root.join(format!("{name}-v2"));
        let p2 = traits_project(&dir2, &edits, false);
        let c2 = checked(&p2, name);
        let (m1, m2) = (c1.model.as_ref().unwrap(), c2.model.as_ref().unwrap());
        let inc = fresh(target, &format!("traits_{name}_inc"));
        let full = fresh(target, &format!("traits_{name}_full"));
        for url in [&inc, &full] {
            ops::init(url, m1, &c1.report, clock()).unwrap();
        }
        // the inherited fields have their columns, and the notes read back from their rows
        let d = dump(&inc);
        let note = &d["note"];
        assert_eq!(note.len(), 12, "{name}");
        assert!(note.iter().all(|r| r.get("n").is_some_and(|v| v.is_some()) && r.contains_key("title")), "{name}: the columns of Doc are missing in note: {:?}", note.first());
        let mut c = Conn::open(&inc).unwrap();
        let unread = c.query("SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" IN ('Note', 'Memo') AND \"version_tx\" = 0", &[]).unwrap();
        assert_eq!(unread[0][0].as_deref(), Some("0"), "{name}: notes and memos read back from their rows");
        drop(c);
        if before {
            as_before_the_fix(&inc);
        }
        let a = ops::deploy(&inc, m2, &c2.report, &dir2, clock(), false, "test deploy");
        let b = ops::deploy_full(&full, m2, &c2.report, &dir2, clock(), false, "test deploy");
        match (&a, &b) {
            (Ok(ra), Ok(_)) => {
                eprintln!("traits {name} on {target}: {:?}", ra.steps);
                if !ra.incremental {
                    failures.push(format!("{name}: not incremental ({:?})", ra.rebuilt_because));
                }
                let (va, vb) = (versions(&inc), versions(&full));
                for (id, v) in &va {
                    if *v != 0 && vb.get(id) != Some(v) {
                        failures.push(format!("{name}: record {id} has version_tx {v}, the rebuild {:?}", vb.get(id)));
                    }
                }
                let (da, db) = (dump(&inc), dump(&full));
                if da != db {
                    let diff: Vec<String> = da.iter().filter(|(t, rows)| db.get(*t) != Some(rows)).map(|(t, rows)| format!("table {t}: {:?}\n vs {:?}", rows.first(), db.get(t).and_then(|r| r.first()))).collect();
                    failures.push(format!("{name}: different databases:\n  {}", diff.join("\n  ")));
                }
                for url in [&inc, &full] {
                    let problems = ops::verify(url, m2, &c2.report, clock()).unwrap().0;
                    if !problems.is_empty() {
                        failures.push(format!("{name}: verify on {url}: {problems:?}"));
                    }
                }
                // what the deploy filled is what the records say: the engine reads it back
                let again = ops::deploy(&inc, m2, &c2.report, &dir2, clock(), false, "again").unwrap();
                if !again.steps.is_empty() {
                    failures.push(format!("{name}: a second deploy did something: {:?}", again.steps));
                }
            }
            _ => failures.push(format!("{name}: unexpected outcome:\n  incremental: {:?}\n  full: {:?}", a.as_ref().map(|r| &r.steps), b.as_ref().map(|r| &r.steps))),
        }
        wipe(&inc);
        wipe(&full);
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn fields_of_a_trait_of_a_trait_on_sqlite() {
    traits_cases("sqlite");
}

#[test]
fn fields_of_a_trait_of_a_trait_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        return;
    };
    traits_cases(&pg);
}
