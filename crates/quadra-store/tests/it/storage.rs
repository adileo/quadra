//! Storage: the example tests run on a real database (reloaded and verified after each
//! test), tampering is detected, and SQLite and PostgreSQL produce the same hash chain.
//!
//! PostgreSQL runs only when `QUADRA_TEST_PG` holds a url (`scripts/pg.sh test` sets it up).

use quadra_runtime::{Ctx, Interp};
use quadra_sema::Project;
use quadra_store::{materialized_formulas, Conn, Database, DbBackend, Sink};
use std::path::{Path, PathBuf};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// Runs an example's tests on `target`; returns the final hash of every test's database.
fn run_on(name: &str, target: &str) -> Vec<(usize, String)> {
    run_with(name, DbBackend::new(target))
}

fn run_with(name: &str, mut backend: DbBackend) -> Vec<(usize, String)> {
    let target = backend.target.clone();
    let project = Project::load(&example(name)).unwrap();
    let results = quadra_runtime::run_tests_with(&project, None, Some(&mut backend))
        .unwrap_or_else(|d| panic!("{name} does not check: {d:#?}"));
    let failed: Vec<String> = results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.name,
                r.location,
                r.message.clone().unwrap_or_default()
            )
        })
        .collect();
    assert!(!results.is_empty(), "{name}: no tests found");
    assert!(
        failed.is_empty(),
        "{name} on {target}:\n{}",
        failed.join("\n")
    );
    backend.hashes
}

#[test]
fn minimal_example_on_sqlite() {
    run_on("minimal", "sqlite");
}

#[test]
fn extended_example_on_sqlite() {
    run_on("extended", "sqlite");
}

/// Loading on demand (a fresh working set per statement, incremental checks and formulas)
/// writes exactly the history of the engine with everything in memory.
#[test]
fn loading_on_demand_writes_the_same_history_as_in_memory() {
    for name in ["minimal", "extended"] {
        let lazy = run_on(name, "sqlite");
        let full = run_with(name, DbBackend::in_memory("sqlite"));
        assert_eq!(lazy, full, "{name}: different histories");
    }
}

#[test]
fn postgres_gives_the_same_hash_chain_as_sqlite() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        eprintln!("QUADRA_TEST_PG not set: PostgreSQL skipped (run scripts/pg.sh test)");
        return;
    };
    for name in ["minimal", "extended"] {
        let lite = run_on(name, "sqlite");
        let post = run_on(name, &pg);
        assert_eq!(
            lite, post,
            "{name}: SQLite and PostgreSQL wrote different histories"
        );
    }
}

const SEED: &str = r#"
seed:
    let anna = Customer(name="Anna Bianchi")
    let pen = Product(code="PEN", name="Pen", price=1.50 EUR/pcs)
    Restock(product=pen, qty=10 pcs)
    let o = Order(customer=anna, lines=[OrderLine(product=pen, qty=4 pcs, discount=10%)])
    o.confirm()
"#;

/// The minimal example plus a seed with data, initialized into a fresh SQLite file.
fn seeded_database(file: &Path) -> (Project, String) {
    let root = example("minimal");
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    let project =
        Project::from_sources(&root, vec![(root.join("main.qd"), format!("{main}{SEED}"))]);
    let url = format!("sqlite:{}", file.display());
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
    }
    (project, url)
}

fn init(project: &Project, url: &str) {
    let checked = quadra_analysis::check(project);
    let model = checked.model.as_ref().expect("model");
    let mut db = Database::open(url, model, &checked.report).unwrap();
    db.init().unwrap();
    let mut interp = Interp::new(model);
    interp.materialize = materialized_formulas(&db.schema);
    interp.sink = Some(Box::new(Sink { db, model }));
    interp.system += 1;
    let seeds = model.seeds.clone();
    interp
        .transaction(|i| {
            for s in &seeds {
                i.stmts(&s.body, &mut Ctx::default())?;
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("seed failed: {e:?}"));
}

fn verify(project: &Project, url: &str) -> Vec<String> {
    let checked = quadra_analysis::check(project);
    let model = checked.model.as_ref().expect("model");
    let mut db = Database::open(url, model, &checked.report).unwrap();
    let mut interp = Interp::new(model);
    interp.store = db.load().unwrap();
    interp.load_units();
    interp.materialize = materialized_formulas(&db.schema);
    interp.materialize_formulas();
    db.verify(model, &interp.store).unwrap()
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("quadra-storage-{}-{name}.db", std::process::id()))
}

#[test]
fn verify_detects_an_altered_table() {
    let file = temp("table");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    assert_eq!(verify(&project, &url), Vec::<String>::new());

    let mut c = Conn::open(&url).unwrap();
    c.batch("UPDATE customer SET credit_limit = '999999'")
        .unwrap();
    drop(c);
    let problems = verify(&project, &url);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("customer.credit_limit") && p.contains("999999")),
        "{problems:?}"
    );
    // the history is intact: only the projection is wrong
    assert!(!problems.iter().any(|p| p.contains("hash")), "{problems:?}");
}

#[test]
fn verify_detects_an_altered_history() {
    let file = temp("history");
    let (project, url) = seeded_database(&file);
    init(&project, &url);

    let mut c = Conn::open(&url).unwrap();
    c.batch(
        "UPDATE _quadra_history SET value = replace(value, '1.5', '0.5') WHERE field = 'price'",
    )
    .unwrap();
    drop(c);
    let problems = verify(&project, &url);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("does not match its hash")),
        "{problems:?}"
    );
}

#[test]
fn verify_detects_a_removed_commit() {
    let file = temp("commit");
    let (project, url) = seeded_database(&file);
    init(&project, &url);

    let mut c = Conn::open(&url).unwrap();
    c.batch("DELETE FROM _quadra_commits").unwrap();
    drop(c);
    let problems = verify(&project, &url);
    assert!(
        problems.iter().any(|p| p.contains("have no commit row")),
        "{problems:?}"
    );
}

#[test]
fn reload_reproduces_the_engine_state() {
    let file = temp("reload");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let model = checked.model.as_ref().unwrap();
    let mut db = Database::open(&url, model, &checked.report).unwrap();
    let store = db.load().unwrap();
    let mut interp = Interp::new(model);
    interp.store = store;
    interp.load_units();
    // formulas computed from the reloaded truth
    let anna = interp.ids("Customer", None)[0];
    let open_total = interp.store.read(anna, "credit_limit", None).unwrap();
    assert_eq!(open_total.to_string(), "1000.00 EUR");
}

// ---- migrations and changes ------------------------------------------------------------

fn shop(version: &str) -> (Project, quadra_analysis::Checked, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/shop")
        .join(version);
    let project = Project::load(&root).unwrap();
    let checked = quadra_analysis::check(&project);
    assert!(
        checked.model.is_some(),
        "{version} does not check: {:#?}",
        checked.diagnostics
    );
    (project, checked, root)
}

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 12, 1)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

/// v1 → migration → change → revert, verified at every step.
fn migrate_and_change(url: &str) {
    use quadra_store::ops;
    let (_, c1, _) = shop("v1");
    ops::init(url, c1.model.as_ref().unwrap(), &c1.report, clock()).unwrap();

    let (_, c2, root) = shop("v2");
    let (m, r) = (c2.model.as_ref().unwrap(), &c2.report);
    // dry run: the report, and nothing written
    let report = ops::migrate(url, m, r, &root, clock(), true)
        .unwrap()
        .expect("something to migrate");
    let lines = report.scripts[0].1.join("\n");
    assert!(
        lines.contains("rename Product.product_kind → Product.type: 3 values"),
        "{lines}"
    );
    assert!(
        lines.contains("backfill Product.hs_code: 3 values, 1 provisional"),
        "{lines}"
    );
    let mut db = Database::open(url, m, r).unwrap();
    assert!(!db.status().unwrap().schema_current);
    drop(db);

    ops::migrate(url, m, r, &root, clock(), false)
        .unwrap()
        .unwrap();
    assert!(
        ops::migrate(url, m, r, &root, clock(), false)
            .unwrap()
            .is_none(),
        "a second migrate has nothing to do"
    );
    assert_eq!(
        ops::verify(url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );

    let mut db = Database::open(url, m, r).unwrap();
    let mut interp = ops::loaded(&mut db, m, clock()).unwrap();
    let products = interp.ids("Product", None);
    let a = products[0];
    assert_eq!(
        interp.store.read(a, "type", None).unwrap().to_string(),
        "Storable"
    );
    assert_eq!(
        interp.store.read(a, "hs_code", None).unwrap().to_string(),
        "73181500"
    );
    // the history written before the rename is read under the new name
    assert_eq!(
        interp.store.read(a, "type", Some(1)).unwrap().to_string(),
        "merce"
    );
    let prov = interp.provisional_values();
    assert_eq!(prov.len(), 1);
    assert_eq!(prov[0].2, "hs_code");
    drop(interp);
    drop(db);

    // change: preview writes nothing; approval rules; apply once; revert
    let p = ops::run_change(url, m, r, "raise_prices", None, clock(), false).unwrap();
    assert_eq!(p.diff.changed.len(), 3);
    assert!(p.tx.is_none());
    let err = ops::run_change(url, m, r, "raise_prices", None, clock(), true).unwrap_err();
    assert!(err.contains("needs the approval of a Manager"), "{err}");
    let err = ops::run_change(url, m, r, "raise_prices", Some("mario"), clock(), true).unwrap_err();
    assert!(err.contains("is not a Manager"), "{err}");
    // a Director is a Manager (`role Director is Manager`)
    let done = ops::run_change(url, m, r, "raise_prices", Some("dora"), clock(), true).unwrap();
    assert!(done.tx.is_some());
    let err = ops::run_change(url, m, r, "raise_prices", Some("lucia"), clock(), true).unwrap_err();
    assert!(err.contains("already applied"), "{err}");
    assert_eq!(
        ops::verify(url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );

    let mut db = Database::open(url, m, r).unwrap();
    let mut interp = ops::loaded(&mut db, m, clock()).unwrap();
    assert_eq!(
        interp
            .store
            .read(a, "list_price", None)
            .unwrap()
            .to_string(),
        "1.10 EUR"
    );
    drop(interp);
    drop(db);

    assert_eq!(
        ops::revert_change(url, m, r, "raise_prices", Some("lucia"), clock()).unwrap(),
        3
    );
    assert!(ops::revert_change(url, m, r, "raise_prices", Some("lucia"), clock()).is_err());
    assert_eq!(
        ops::verify(url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );
    let mut db = Database::open(url, m, r).unwrap();
    let mut interp = ops::loaded(&mut db, m, clock()).unwrap();
    assert_eq!(
        interp
            .store
            .read(a, "list_price", None)
            .unwrap()
            .to_string(),
        "1.00 EUR"
    );
    let changes = ops::list_changes(url, m, r, clock()).unwrap();
    assert!(changes[0].applied_tx.is_some() && changes[0].reverted_tx.is_some());
}

#[test]
fn migration_and_change_on_sqlite() {
    let file = temp("shop");
    let _ = std::fs::remove_file(&file);
    migrate_and_change(&format!("sqlite:{}", file.display()));
}

#[test]
fn migration_and_change_on_postgres() {
    let Ok(pg) = std::env::var("QUADRA_TEST_PG") else {
        return;
    };
    let schema = format!("shop_{}", std::process::id());
    let _ = Conn::open(&pg)
        .and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
    migrate_and_change(&format!("{pg}#schema={schema}"));
    let _ = Conn::open(&pg)
        .and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
}

#[test]
fn a_value_the_migration_cannot_convert_stops_it() {
    use quadra_store::ops;
    let file = temp("shop-bad");
    let _ = std::fs::remove_file(&file);
    let url = format!("sqlite:{}", file.display());
    let (_, _, root1) = shop("v1");
    // v1 with one more product of an unknown kind
    let main = std::fs::read_to_string(root1.join("main.qd")).unwrap();
    let main = format!("{main}    Product(code=\"D\", name=\"Gift\", product_kind=\"omaggio\", list_price=0.00 EUR)\n");
    let p = Project::from_sources(&root1, vec![(root1.join("main.qd"), main)]);
    let c1b = quadra_analysis::check(&p);
    ops::init(&url, c1b.model.as_ref().unwrap(), &c1b.report, clock()).unwrap();
    let (_, c2, root) = shop("v2");
    let err = ops::migrate(
        &url,
        c2.model.as_ref().unwrap(),
        &c2.report,
        &root,
        clock(),
        true,
    )
    .unwrap_err();
    assert!(err.contains("Unknown product kind: omaggio"), "{err}");
    // nothing was written
    let mut db = Database::open(&url, c2.model.as_ref().unwrap(), &c2.report).unwrap();
    assert_eq!(db.status().unwrap().commits, 1);
}

// ---- concurrency ---------------------------------------------------------------------------

/// An engine over the database, loading on demand at the current head.
fn engine<'m>(
    url: &str,
    model: &'m quadra_ir::Model,
    report: &'m quadra_types::Report,
) -> Interp<'m> {
    engine_with(url, model, report, None)
}

type SharedCache = std::rc::Rc<std::cell::RefCell<quadra_store::Cache>>;

fn engine_with<'m>(
    url: &str,
    model: &'m quadra_ir::Model,
    report: &'m quadra_types::Report,
    cache: Option<SharedCache>,
) -> Interp<'m> {
    let db = Database::open(url, model, report).unwrap();
    let schema = db.schema.clone();
    let src = quadra_store::DbSource::open(url, schema, cache).unwrap();
    let mut i = Interp::new(model);
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model }));
    i.load_units();
    i
}

fn first(i: &mut Interp, ent: &str) -> quadra_runtime::V {
    let id = i.ids(ent, None)[0];
    quadra_runtime::V::Ref {
        ent: ent.into(),
        id,
    }
}

fn amount(v: quadra_runtime::V) -> String {
    match v {
        quadra_runtime::V::Price { amt, .. } => amt.normalize().to_string(),
        other => other.to_string(),
    }
}

fn eur(s: &str) -> quadra_runtime::V {
    quadra_runtime::V::Price {
        amt: s.parse().unwrap(),
        cur: "EUR".into(),
        per: "pcs".into(),
    }
}

#[test]
fn a_commit_is_refused_when_others_changed_what_it_read() {
    let file = temp("conflict");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);

    // two engines read the same product; the first one commits a new price
    let mut a = engine(&url, m, r);
    let mut b = engine(&url, m, r);
    let pa = first(&mut a, "Product");
    let pb = first(&mut b, "Product");
    a.transaction(|i| i.set(&pa, "price", eur("2.00"), None))
        .unwrap();
    let err = b
        .transaction(|i| i.set(&pb, "price", eur("3.00"), None))
        .unwrap_err();
    let quadra_runtime::Flow::Conflict(c) = err else {
        panic!("expected a conflict, got {err:?}")
    };
    assert_eq!(c.len(), 1);
    assert!(
        c[0].what
            .iter()
            .any(|w| w.starts_with("Product#") && w.ends_with(".price")),
        "{c:?}"
    );

    // after seeing the change, a fresh engine can commit
    let mut b = engine(&url, m, r);
    let pb = first(&mut b, "Product");
    b.transaction(|i| i.set(&pb, "price", eur("3.00"), None))
        .unwrap();
    assert_eq!(
        quadra_store::ops::verify(&url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );
}

#[test]
fn independent_commits_both_succeed() {
    let file = temp("independent");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);

    let mut a = engine(&url, m, r);
    let mut b = engine(&url, m, r);
    let customer = first(&mut a, "Customer");
    let product = first(&mut b, "Product");
    a.transaction(|i| {
        i.set(
            &customer,
            "name",
            quadra_runtime::V::Text("Anna B.".into()),
            None,
        )
    })
    .unwrap();
    // b started before a committed, but read nothing a changed: it gets the next number
    b.transaction(|i| {
        i.set(
            &product,
            "name",
            quadra_runtime::V::Text("Blue pen".into()),
            None,
        )
    })
    .unwrap();
    assert_eq!(b.store.tx, a.store.tx + 1);
    assert_eq!(
        quadra_store::ops::verify(&url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );
}

#[test]
fn concurrent_movements_of_the_same_stock_are_serialized() {
    let file = temp("stock");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);

    let qty = |n: i64| quadra_runtime::V::Qty {
        v: n.into(),
        unit: "pcs".into(),
    };
    let mut a = engine(&url, m, r);
    let mut b = engine(&url, m, r);
    let pa = first(&mut a, "Product");
    let pb = first(&mut b, "Product");
    a.transaction(|i| {
        i.create(
            "Restock",
            vec![("product".into(), pa.clone()), ("qty".into(), qty(5))],
        )
        .map(|_| ())
    })
    .unwrap();
    // b's stock check read the rows of the same product: refused, with what changed
    let err = b
        .transaction(|i| {
            i.create(
                "Restock",
                vec![("product".into(), pb.clone()), ("qty".into(), qty(3))],
            )
            .map(|_| ())
        })
        .unwrap_err();
    let quadra_runtime::Flow::Conflict(c) = err else {
        panic!("expected a conflict, got {err:?}")
    };
    assert!(
        c[0].what.iter().any(|w| w.starts_with("Stock rows")),
        "{c:?}"
    );
    let mut b = engine(&url, m, r);
    let pb = first(&mut b, "Product");
    b.transaction(|i| {
        i.create(
            "Restock",
            vec![("product".into(), pb.clone()), ("qty".into(), qty(3))],
        )
        .map(|_| ())
    })
    .unwrap();
    let stock = b.get(&pb, "stock", None, None).unwrap();
    assert_eq!(stock.to_string(), "18 pcs"); // 10 + 5 + 3
    assert_eq!(
        quadra_store::ops::verify(&url, m, r, clock()).unwrap().0,
        Vec::<String>::new()
    );
}

#[test]
fn the_cache_is_reused_and_never_stale() {
    let file = temp("cache");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let cache: SharedCache = std::rc::Rc::new(std::cell::RefCell::new(quadra_store::Cache::new(
        1 << 20,
        std::time::Duration::from_secs(60),
    )));

    let mut a = engine_with(&url, m, r, Some(cache.clone()));
    let p = first(&mut a, "Product");
    a.get(&p, "price", None, None).unwrap();
    let misses = cache.borrow().misses;
    let mut b = engine_with(&url, m, r, Some(cache.clone()));
    let pb = first(&mut b, "Product");
    assert_eq!(amount(b.get(&pb, "price", None, None).unwrap()), "1.5");
    assert!(
        cache.borrow().hits > 0,
        "the second engine reads from the cache"
    );
    assert_eq!(cache.borrow().misses, misses);

    // a commit elsewhere: the next engine sees the new value, not the cached one
    let mut w = engine(&url, m, r);
    let pw = first(&mut w, "Product");
    w.transaction(|i| i.set(&pw, "price", eur("2.00"), None))
        .unwrap();
    let mut c = engine_with(&url, m, r, Some(cache.clone()));
    let pc = first(&mut c, "Product");
    assert_eq!(amount(c.get(&pc, "price", None, None).unwrap()), "2");

    // prewarm: recent records up to the budget
    let fresh = std::rc::Rc::new(std::cell::RefCell::new(quadra_store::Cache::new(
        1 << 20,
        std::time::Duration::from_secs(60),
    )));
    let mut conn = Conn::open(&url).unwrap();
    let n = fresh
        .borrow_mut()
        .prewarm(
            &mut conn,
            chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .unwrap();
    assert!(n > 20 && fresh.borrow().len() == n, "{n}");
}

/// A one-file project from `src`, run on SQLite (loading on demand) and checked against the
/// engine after every test (regressions of the known issues).
fn run_source_on_sqlite(src: &str) {
    let root = Path::new("/regression");
    let project = Project::from_sources(root, vec![(root.join("main.qd"), src.to_string())]);
    let mut backend = DbBackend::new("sqlite");
    let results = quadra_runtime::run_tests_with(&project, None, Some(&mut backend))
        .unwrap_or_else(|d| {
            let msgs: Vec<String> = d.iter().map(|d| d.message.clone()).collect();
            panic!("does not check:\n{}", msgs.join("\n"))
        });
    let failed: Vec<String> = results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.name,
                r.location,
                r.message.clone().unwrap_or_default()
            )
        })
        .collect();
    assert!(!results.is_empty(), "no tests found");
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// A materialized formula through `group_by(...).map(g => g.items...)` is recomputed when a
/// grouped record changes.
#[test]
fn formulas_through_group_by_stay_fresh_on_the_database() {
    run_source_on_sqlite(
        r#"
entity Order:
    lines: [OrderLine]
    by_rate = lines.group_by(l => l.rate).map(g => (rate: g.key, base: g.items.sum(l => l.amount)))
    total_by_rate = by_rate.sum(r => r.base)
    by_product = lines.group_by(l => l.product).map(g => g.key.name)
    first_amounts = lines.group_by(l => l.rate).map(g => (head: g.items.first())).map(t => t.head?.amount)

entity Product:
    name: Text

entity OrderLine:
    order: Order parent
    product: Product
    rate: Percent default 22%
    amount: Money<EUR>

test "group_by":
    let p = Product(name="Pen")
    let o = Order(lines=[OrderLine(product=p, amount=1.00 EUR)])
    expect o.total_by_rate == 1.00 EUR
    o.lines[0].amount = 2.00 EUR
    expect o.total_by_rate == 2.00 EUR
    expect o.first_amounts == [2.00 EUR]
    p.name = "Pencil"
    expect o.by_product == ["Pencil"]
"#,
    );
}

/// Trait rules hold among the records of each implementing entity, in memory as in the
/// database (where a trait's `unique` is a constraint of each implementor's table).
#[test]
fn trait_rules_agree_between_memory_and_database() {
    run_source_on_sqlite(
        r#"
trait Doc:
    n: Code
    unique (n)
    check n.len() > 1, "n is too short"

trait Numbered is Doc:
    seq: Int default 1

entity Invoice is Doc:
    amount: Int default 0

entity Note is Numbered:
    body: Text default ""

test "trait rules":
    Invoice(n="AB")
    expect_error "AB already exists":
        Invoice(n="AB")
    Note(n="AB")
    expect_error "AB already exists":
        Note(n="AB")
    expect_error "n is too short":
        Note(n="A")
    let i = Invoice(n="CD")
    expect_error "AB already exists":
        i.n = "AB"
"#,
    );
}

/// The rules of a `record` held by a field are enforced on the database too.
#[test]
fn record_rules_on_the_database() {
    run_source_on_sqlite(
        r#"
record Address:
    zip: Text where z => z.len() == 5
    country: Text default "IT"
    province: Text? required when country == "IT"

entity Customer:
    name: Text
    address: Address?

test "record rules":
    let c = Customer(name="Anna", address=Address(zip="20100", province="MI"))
    expect_error "Address.zip = 1: does not satisfy its `where`":
        c.address = Address(zip="1", province="MI")
    expect_error "Address.province is required":
        Customer(name="Bea", address=Address(zip="20100"))
    c.address = Address(zip="75001", country="FR")
    expect c.address?.zip == "75001"
"#,
    );
}

/// Typed zeros (`default 0` on a price, an empty sum of amounts) are stored as such.
#[test]
fn typed_zeros_on_the_database() {
    run_source_on_sqlite(
        r#"
entity Product:
    name: Text
    price: Price<EUR, pcs> where > 0
    list: Price<EUR, pcs> default 0
    stock: Qty<uom> default 0
    uom: Uom default pcs
    lines: [Line]
    total = lines.sum(l => l.amount)

entity Line:
    product: Product parent
    amount: Money<EUR>

test "zeros":
    let p = Product(name="Pen", price=7.50 EUR/pcs)
    expect f"{p.list} {p.stock} {p.total}" == "0.00 EUR/pcs 0 pcs 0.00 EUR"
    Line(product=p, amount=2.00 EUR)
    expect p.total == 2.00 EUR
"#,
    );
}

/// `emit` writes the event to `_quadra_outbox` in the same database transaction as the
/// commit; a transaction that fails writes nothing. Delivered events are marked.
#[test]
fn emitted_events_are_written_to_the_outbox_with_the_commit() {
    let src = r#"
event Notice(to: Email, n: Int, doc: Doc)

entity Doc:
    n: Int
    action send:
        emit Notice(to="a@example.com", n=n, doc=self)
    action broken:
        emit Notice(to="b@example.com", n=n, doc=self)
        fail "refused"
"#;
    let root = Path::new("/outbox");
    let project = Project::from_sources(root, vec![(root.join("main.qd"), src.to_string())]);
    let checked = quadra_analysis::check(&project);
    let (model, report) = (checked.model.as_ref().expect("model"), &checked.report);
    let file = temp("outbox");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
    }
    let url = format!("sqlite:{}", file.display());
    quadra_store::ops::init(&url, model, report, clock()).unwrap();
    let mut i = engine(&url, model, report);
    let doc = i
        .transaction(|i| i.create("Doc", vec![("n".into(), quadra_runtime::V::Int(7))]))
        .unwrap();
    let action = |name: &str| {
        model
            .entity("Doc")
            .unwrap()
            .actions
            .iter()
            .find(|a| a.name == name)
    };
    i.transaction(|i| i.run_action(&doc, "Doc", "send", action("send"), vec![], vec![]))
        .unwrap();
    assert!(i
        .transaction(|i| i.run_action(&doc, "Doc", "broken", action("broken"), vec![], vec![]))
        .is_err());
    let pending = quadra_store::ops::outbox(&url, model, report, true).unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let e = &pending[0];
    assert_eq!(e.event, "Notice");
    assert_eq!(e.fields["to"], serde_json::json!("a@example.com"));
    assert_eq!(e.fields["n"], serde_json::json!(7));
    assert_eq!(e.fields["doc"]["ent"], serde_json::json!("Doc"));
    assert!(e.delivered_at.is_none());
    let marked =
        quadra_store::ops::outbox_delivered(&url, model, report, &[e.id, 999], clock()).unwrap();
    assert_eq!(marked, 1);
    assert!(quadra_store::ops::outbox(&url, model, report, true)
        .unwrap()
        .is_empty());
    let all = quadra_store::ops::outbox(&url, model, report, false).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].delivered_at.is_some());
    assert!(verify(&project, &url).is_empty());
}

/// `on X created` / `on X changes` reactions and the plain checks of a ledger behave on the
/// database (loading on demand) as in memory.
#[test]
fn record_reactions_and_ledger_row_checks_on_the_database() {
    run_source_on_sqlite(
        r#"
ledger Moves:
    key code: Code
    measure qty: Int
    check qty != 0, "A move of nothing"

entity Item:
    code: Code
    qty: Int default 0
    edits: Int default 0
    state: enum(Draft, Done, Cancelled) default Draft
    post to Moves when state is Done:
        add(code=code, qty=qty)

entity Audit:
    item: Item
    what: Text

on Item created as i:
    Audit(item=i, what="created")

on Item changes as i:
    i.edits += 1

test "reactions and row checks":
    let a = Item(code="A", qty=2)
    expect Audit.all().count() == 1
    a.state = Done
    expect a.edits == 1
    expect Moves.all().sum(m => m.qty) == 2
    let b = Item(code="B")
    expect_error "A move of nothing":
        b.state = Done
    expect b.edits == 0
    a.state = Cancelled
    expect a.edits == 2
    expect Moves.all().sum(m => m.qty) == 0
"#,
    );
}

/// Items 16–18 on the database: a document reopened by a transition is editable again (the
/// unfreezing is written), its lines read the data as of the freezing, and a frozen line's
/// `where` is not checked again against later data.
#[test]
fn frozen_documents_and_their_lines_on_the_database() {
    run_source_on_sqlite(
        r#"
entity Product:
    name: Text
    price: Int
    active: Bool default true

entity Order:
    notes: Text default ""
    lines: [OrderLine]
    state: enum(Draft, Confirmed) default Draft
    total = lines.sum(l => l.amount)
    frozen when state is not Draft
    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Draft by reopen
    action reopen:
        notes = "reopened"

entity OrderLine:
    order: Order parent
    product: Product where p => p.active
    qty: Int
    amount = qty * product.price
    product_name = product.name
    current_price = product.price
    live current_price

test "frozen lines":
    let p = Product(name="Pen", price=2)
    let q = Product(name="Ink", price=1)
    let o = Order(lines=[OrderLine(product=p, qty=3), OrderLine(product=q, qty=1)])
    let l = o.lines.first() ?? fail "no line"
    o.confirm()
    p.name = "Blue pen"
    p.price = 5
    q.active = false
    expect l.product_name == "Pen"
    expect o.total == 7
    expect l.current_price == 5
    expect_error "its OrderLine lines cannot change":
        l.qty = 4
    expect_error "OrderLine.product = ":
        o.reopen()
    q.active = true
    o.reopen()
    expect o.notes == "reopened"
    expect l.product_name == "Blue pen"
    expect o.total == 16
    l.qty = 4
    expect o.total == 21
"#,
    );
}

/// Rollups read from their stored values (phase 3) over documents that are frozen and
/// unfrozen: a frozen order's total stays as of its freezing while its products change; its
/// customer's rollup follows when it is unfrozen (no field of the customer or of its orders
/// was written: only the order's view of the world changed).
#[test]
fn rollups_over_frozen_documents_follow_their_unfreezing() {
    run_source_on_sqlite(
        r#"
entity Product:
    name: Text
    price: Int

entity Customer:
    name: Text
    orders: [Order]
    total = orders.sum(o => o.total)
    biggest = orders.max(o => o.total)
    count = orders.count(o => o.total > 5)

entity Order:
    customer: Customer
    lines: [OrderLine]
    state: enum(Draft, Confirmed) default Draft
    total = lines.sum(l => l.amount)
    frozen when state is not Draft
    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Draft by reopen

entity OrderLine:
    order: Order parent
    product: Product
    qty: Int
    amount = qty * product.price

test "frozen and unfrozen":
    let p = Product(name="Pen", price=2)
    let c = Customer(name="Rossi")
    let o = Order(customer=c, lines=[OrderLine(product=p, qty=3)])
    let o2 = Order(customer=c, lines=[OrderLine(product=p, qty=1)])
    o.confirm()
    expect c.total == 8
    p.price = 5
    expect o.total == 6
    expect o2.total == 5
    expect c.total == 11
    expect c.biggest == 6
    expect c.count == 1
    o.reopen()
    expect o.total == 15
    expect c.total == 20
    expect c.biggest == 15
    expect c.count == 1
    o.confirm()
    p.price = 1
    expect c.total == 16
    expect c.count == 1
"#,
    );
}

/// `min` and `max` rollups kept by delta (phase 3): when the extreme leaves (lowered, moved
/// away, filtered out, tied), the extreme of the other children is looked up in SQL.
#[test]
fn extremes_of_rollups_follow_their_children() {
    run_source_on_sqlite(
        r#"
entity Region:
    name: Text
    customers: [Customer]
    best: Money<EUR>? = customers.max(c => c.revenue)
    worst: Money<EUR>? = customers.where(c => c.active).min(c => c.revenue)
    first: Date? = customers.min(c => c.since)
    n = customers.count(c => c.active)

entity Customer:
    name: Text
    region: Region
    since: Date
    active: Bool default true
    orders: [Order]
    revenue: Money<EUR> = orders.sum(o => o.amount)

entity Order:
    customer: Customer
    amount: Money<EUR>

test "extremes":
    let north = Region(name="North")
    let south = Region(name="South")
    let a = Customer(name="A", region=north, since=2026-01-05)
    let b = Customer(name="B", region=north, since=2026-01-02)
    let c = Customer(name="C", region=north, since=2026-01-09)
    let oa = Order(customer=a, amount=10.00 EUR)
    let ob = Order(customer=b, amount=30.00 EUR)
    let oc = Order(customer=c, amount=20.00 EUR)
    expect north.best == 30.00 EUR
    expect north.worst == 10.00 EUR
    expect north.first == 2026-01-02
    ob.amount = 5.00 EUR
    expect north.best == 20.00 EUR
    expect north.worst == 5.00 EUR
    b.region = south
    expect north.best == 20.00 EUR
    expect north.worst == 10.00 EUR
    expect north.first == 2026-01-05
    expect south.best == 5.00 EUR
    a.active = false
    expect north.worst == 20.00 EUR
    expect north.n == 1
    oc.amount = 10.00 EUR
    expect north.best == 10.00 EUR
    expect north.worst == 10.00 EUR
    a.active = true
    oa.amount = 10.00 EUR
    oc.amount = 1.00 EUR
    expect north.best == 10.00 EUR
    expect north.worst == 1.00 EUR
    c.region = south
    a.region = south
    expect north.best == none
    expect north.first == none
    expect south.best == 10.00 EUR
    expect south.first == 2026-01-02
"#,
    );
}

/// Items 22, 31, 33, 35 and 36 on a database loaded on demand: `updated_at`/`updated_by`
/// from the commits, a gapless sequence after a failed transaction, rows posted earlier in a
/// transaction, the one-field `unique` message, quantities stored in the field's unit.
#[test]
fn engine_fixes_hold_on_the_database() {
    run_source_on_sqlite(
        r#"
ledger Stock:
    key code: Code
    measure qty: Int

entity Receipt:
    code: Code
    qty: Int
    post to Stock:
        add(code=code, qty=qty)

entity Order:
    code: Code
    qty: Int
    weight: Qty<kg> default 0
    state: enum(Draft, Confirmed) default Draft
    flow state:
        Draft -> Confirmed by confirm
    available = Stock.where(s => s.code == code).sum(s => s.qty)

before Order.confirm as o:
    require o.available >= o.qty, f"only {o.available} available"

entity Invoice:
    n: Code default next_number(current.company, "INV", on=2026-01-10)
    ref: Code?
    amount: Int
    unique (ref)
    check amount > 0, "empty invoice"

fixture stocked:
    clock 2026-11-02 09:00
    User(login="ann")
    let c = Company(name="Acme", currency=EUR)
    login("admin", company=c)
    Sequence(code="INV", pattern="INV/{year}/{n:03}")
    Receipt(code="A", qty=5)
    let o = Order(code="A", qty=3, weight=500 g)
    o.confirm()

test "on the database":
    use stocked
    let o = Order.all().first(x => x.code == "A")
    expect o?.state is Confirmed
    expect f"{o?.weight}" == "0.5 kg"
    expect o?.updated_at == 2026-11-02 09:00
    clock 2026-11-05 12:00
    login("ann")
    o?.qty = 2
    expect o?.updated_at == 2026-11-05 12:00
    expect o?.updated_by?.login == "ann"
    expect o?.created_at == 2026-11-02 09:00
    let a = Invoice(amount=1, ref="R1")
    expect a.n == "INV/2026/001"
    expect_error "empty invoice":
        Invoice(amount=0)
    expect Invoice(amount=2).n == "INV/2026/002"
    expect_error "Invoice.ref must be unique: R1 already exists":
        Invoice(amount=3, ref="R1")
"#,
    );
}

/// 27. A sweep that changes nothing writes no commit.
#[test]
fn an_idle_sweep_writes_no_commit() {
    let file = temp("sweep");
    let (project, url) = seeded_database(&file);
    init(&project, &url);
    let checked = quadra_analysis::check(&project);
    let model = checked.model.as_ref().unwrap();
    let commits = |url: &str| {
        Database::open(url, model, &checked.report)
            .unwrap()
            .status()
            .unwrap()
            .commits
    };
    let before = commits(&url);
    assert_eq!(
        quadra_store::ops::sweep(&url, model, &checked.report, clock()).unwrap(),
        None
    );
    assert_eq!(
        quadra_store::ops::sweep(&url, model, &checked.report, clock()).unwrap(),
        None
    );
    assert_eq!(commits(&url), before, "an idle sweep wrote a commit");
    assert!(verify(&project, &url).is_empty());
}

/// A rule the existing data breaks names the record: on a deploy nobody is editing it.
#[test]
fn a_new_rule_broken_by_the_data_names_the_record() {
    use quadra_store::ops;
    let dir = std::env::temp_dir().join(format!("quadra-storage-{}-rule", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let model = |extra: &str| {
        let src = format!("entity TimeLog:\n    hours: Duration\n    date: Date\n{extra}\nfixture demo:\n    TimeLog(hours=1 hour, date=2026-12-24)\n");
        let project = Project::from_sources(&dir, vec![(dir.join("main.qd"), src)]);
        let checked = quadra_analysis::check(&project);
        assert!(checked.model.is_some(), "{:#?}", checked.diagnostics);
        checked
    };
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let v1 = model("");
    ops::init(&url, v1.model.as_ref().unwrap(), &v1.report, clock()).unwrap();
    ops::fixture(
        &url,
        v1.model.as_ref().unwrap(),
        &v1.report,
        "demo",
        clock(),
    )
    .unwrap();
    // `hours > 0` holds (a duration compares with 0); the date is in the future
    let v2 = model("    check hours > 0, \"Hours must be greater than 0\"\n    check date <= today(), \"Date must be today or in the past\"\n");
    let e = ops::deploy(
        &url,
        v2.model.as_ref().unwrap(),
        &v2.report,
        &dir,
        clock(),
        true,
        "test",
    )
    .unwrap_err();
    // the rule's line, the record with the value that breaks it, and what can be done
    for part in [
        "main.qd:5\n",
        "check date <= today(), \"Date must be today or in the past\"",
        "1 TimeLog breaks it:",
        "TimeLog#",
        "date = 2026-12-24",
        "add a migration that fixes them",
    ] {
        assert!(e.contains(part), "missing {part:?} in:\n{e}");
    }
    assert!(!e.contains("Hours must be greater than 0"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `becomes` condition that reads the clock fires at the first sweep after it turns true,
/// once: a sweep compares it with what it was at the previous sweep, not with the current
/// clock (the record has not changed since it was written).
#[test]
fn a_reminder_fires_at_the_first_sweep_after_its_time() {
    let dir = std::env::temp_dir().join(format!("quadra-storage-{}-reminder", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"r\"\n").unwrap();
    std::fs::write(
        dir.join("main.qd"),
        "entity Reminder:\n    at: DateTime\n\nentity Fired:\n    reminder: Reminder\n\non Reminder becomes at <= now() as r:\n    Fired(reminder=r)\n\nfixture demo:\n    Reminder(at=2026-12-01T10:01)\n",
    )
    .unwrap();
    let project = Project::load(&dir).unwrap();
    let checked = quadra_analysis::check(&project);
    let model = checked.model.as_ref().unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    let t = |s: i64| clock() + chrono::Duration::seconds(s);
    quadra_store::ops::init(&url, model, &checked.report, t(0)).unwrap();
    quadra_store::ops::fixture(&url, model, &checked.report, "demo", t(0)).unwrap();
    let fired = || {
        Conn::open(&url)
            .unwrap()
            .query("SELECT count(*) FROM \"fired\"", &[])
            .unwrap()[0][0]
            .clone()
            .unwrap_or_default()
    };
    let sweep = |s: i64| quadra_store::ops::sweep(&url, model, &checked.report, t(s)).unwrap();
    assert_eq!(sweep(30), None, "not due yet");
    assert_eq!(fired(), "0");
    assert!(
        sweep(90).is_some(),
        "due since 10:01: the sweep at 10:01:30 fires it"
    );
    assert_eq!(fired(), "1");
    assert_eq!(sweep(150), None, "fired once");
    assert_eq!(fired(), "1");
    // the first sweep ever, long after: it fires too (compared with when it was written)
    let _ = std::fs::remove_file(dir.join("data.db"));
    quadra_store::ops::init(&url, model, &checked.report, t(0)).unwrap();
    quadra_store::ops::fixture(&url, model, &checked.report, "demo", t(0)).unwrap();
    assert!(sweep(3600).is_some());
    assert_eq!(fired(), "1");
    assert!(verify(&project, &url).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A migration's `load from` reads a file of the project only: no absolute path, no `..`, no link.
#[test]
fn load_from_reads_only_a_file_of_the_project() {
    use quadra_store::ops;
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/shop/v2");
    for (what, from, link) in [
        ("absolute", "/etc/hosts", false),
        ("parent", "../customs.csv", false),
        ("link", "link.csv", true),
    ] {
        let root =
            std::env::temp_dir().join(format!("quadra-loadfrom-{}-{what}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("migrations")).unwrap();
        for f in ["customs.csv", "main.qd", "quadra.toml"] {
            std::fs::copy(src.join(f), root.join(f)).unwrap();
        }
        let m = std::fs::read_to_string(src.join("migrations/2026-12-01_customs.qd")).unwrap();
        std::fs::write(
            root.join("migrations/2026-12-01_customs.qd"),
            m.replace(
                "load from \"customs.csv\"",
                &format!("load from \"{from}\""),
            ),
        )
        .unwrap();
        if link {
            std::os::unix::fs::symlink("/etc/hosts", root.join("link.csv")).unwrap();
        }
        let (_, c1, _) = shop("v1");
        let url = format!("sqlite:{}", root.join("data.db").display());
        ops::init(&url, c1.model.as_ref().unwrap(), &c1.report, clock()).unwrap();
        let project = Project::load(&root).unwrap();
        let c2 = quadra_analysis::check(&project);
        let err = ops::migrate(
            &url,
            c2.model.as_ref().unwrap(),
            &c2.report,
            &root,
            clock(),
            false,
        )
        .unwrap_err();
        assert!(
            err.contains("a file of the project")
                || err.contains("is a link")
                || err.contains("outside the project"),
            "{what}: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
