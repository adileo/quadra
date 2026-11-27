//! The sequencer: commits from many threads at once are numbered, checked against each other
//! in memory, written in batches, and give the same hash chain as committing one at a time.

use quadra_runtime::{Ctx, Flow, Interp, V};
use quadra_sema::Project;
use quadra_store::sequencer::{Batch, DurabilitySink, Options, Rule};
use quadra_store::{materialized_formulas, Database, DbSource, SequencedSink, Sequencer, Sink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SEED: &str = r#"
seed:
    let anna = Customer(name="Anna Bianchi", credit_limit=1000000.00 EUR)
    let pen = Product(code="PEN", name="Pen", price=1.50 EUR/pcs)
    Restock(product=pen, qty=10 pcs)
"#;

fn project() -> Project {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    Project::from_sources(&root, vec![(root.join("main.qd"), format!("{main}{SEED}"))])
}

/// A fresh database: SQLite, or a schema of its own on PostgreSQL when `QUADRA_SEQ_PG` holds
/// a url (`scripts/pg.sh start` prints one).
fn fresh(name: &str) -> String {
    if let Ok(pg) = std::env::var("QUADRA_SEQ_PG") {
        let schema = format!("seq_{name}_{}", std::process::id());
        let _ = quadra_store::Conn::open(&pg).and_then(|mut c| c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")));
        return format!("{pg}#schema={schema}");
    }
    let file: PathBuf = std::env::temp_dir().join(format!("quadra-seq-{}-{name}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
    }
    format!("sqlite:{}", file.display())
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
    interp.transaction(|i| {
        for s in &seeds {
            i.stmts(&s.body, &mut Ctx::default())?;
        }
        Ok(())
    })
    .unwrap_or_else(|e| panic!("seed failed: {e:?}"));
}

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 12, 1).unwrap().and_hms_opt(10, 0, 0).unwrap()
}

/// An engine over the database at its current head; commits through `sequencer` when given.
fn engine<'m>(url: &str, model: &'m quadra_ir::Model, report: &'m quadra_types::Report, sequencer: Option<&Sequencer>) -> Interp<'m> {
    let db = Database::open(url, model, report).unwrap();
    let schema = db.schema.clone();
    let src = DbSource::open(url, schema, None).unwrap();
    let mut i = Interp::new(model);
    i.set_clock(clock());
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(match sequencer {
        Some(s) => Box::new(SequencedSink { db, model, sequencer: s.clone(), tickets: None }),
        None => Box::new(Sink { db, model }),
    });
    i.load_units();
    i
}

fn first(i: &mut Interp, ent: &str) -> V {
    let id = i.ids(ent, None)[0];
    V::Ref { ent: ent.into(), id }
}

fn verify(project: &Project, url: &str) -> Vec<String> {
    let checked = quadra_analysis::check(project);
    quadra_store::ops::verify(url, checked.model.as_ref().unwrap(), &checked.report, clock()).unwrap().0
}

fn count(url: &str, sql: &str) -> i64 {
    let mut c = quadra_store::Conn::open(url).unwrap();
    c.query(sql, &[]).unwrap()[0][0].as_deref().unwrap_or("0").parse().unwrap()
}

/// What a destination received, batch by batch.
#[derive(Clone, Default)]
struct Kept(Arc<Mutex<Vec<(u64, u64, usize)>>>);

impl DurabilitySink for Kept {
    fn persist(&mut self, b: &Batch) -> Result<(), String> {
        self.0.lock().unwrap().push((b.first_tx, b.last_tx, b.statements.len()));
        Ok(())
    }
}

#[test]
fn commits_from_many_threads_are_batched_and_verified() {
    let p = project();
    let url = fresh("many");
    init(&p, &url);
    let kept = Kept::default();
    let options = Options { destinations: vec![Box::new(kept.clone())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (url, s) = (url.clone(), s.clone());
            std::thread::spawn(move || {
                let p = project();
                let checked = quadra_analysis::check(&p);
                let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
                for n in 0..25 {
                    // a fresh engine each time, as a request has; run again on a conflict, as the
                    // API does (the name is unique: every creation reads the other names)
                    let name = V::Text(format!("customer {t}-{n}").into());
                    let mut tries = 0;
                    loop {
                        let mut i = engine(&url, m, r, Some(&s));
                        match i.transaction(|i| i.create("Customer", vec![("name".into(), name.clone())]).map(|_| ())) {
                            Ok(()) => break,
                            Err(Flow::Conflict(_)) if tries < 50 => tries += 1,
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    s.stop();
    assert_eq!(count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"customer\""), 201);
    assert_eq!(verify(&p, &url), Vec::<String>::new());
    let commits = count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_commits\"");
    let batches = count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_batches\"");
    assert_eq!(commits, 201, "the seed and 200 commits");
    // (how many share a batch depends on how many arrive together: the load test measures it)
    assert!(batches <= 200, "{batches} batches for 200 commits");
    eprintln!("{batches} batches for 200 commits");
    let kept = kept.0.lock().unwrap();
    // (batches written while one is on its way go together: as many segments at most)
    assert!(kept.len() as i64 <= batches, "{} segments for {batches} batches", kept.len());
    assert_eq!(kept.iter().map(|(a, b, _)| b - a + 1).sum::<u64>(), 200);
}

#[test]
fn a_conflict_is_found_in_memory() {
    let p = project();
    let url = fresh("conflict");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let price = |x: &str| V::Price { amt: x.parse().unwrap(), cur: "EUR".into(), per: "pcs".into() };
    let mut a = engine(&url, m, r, Some(&s));
    let mut b = engine(&url, m, r, Some(&s));
    let pa = first(&mut a, "Product");
    let pb = first(&mut b, "Product");
    a.transaction(|i| i.set(&pa, "price", price("2.00"), None)).unwrap();
    let err = b.transaction(|i| i.set(&pb, "price", price("3.00"), None)).unwrap_err();
    let Flow::Conflict(c) = err else { panic!("expected a conflict, got {err:?}") };
    assert!(c[0].what.iter().any(|w| w.starts_with("Product#") && w.ends_with(".price")), "{c:?}");
    // read again, it commits
    let mut b = engine(&url, m, r, Some(&s));
    let pb = first(&mut b, "Product");
    b.transaction(|i| i.set(&pb, "price", price("3.00"), None)).unwrap();
    s.stop();
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn independent_commits_from_the_same_snapshot_both_succeed() {
    let p = project();
    let url = fresh("independent");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let mut a = engine(&url, m, r, Some(&s));
    let mut b = engine(&url, m, r, Some(&s));
    let customer = first(&mut a, "Customer");
    let product = first(&mut b, "Product");
    a.transaction(|i| i.set(&customer, "name", V::Text("Anna B.".into()), None)).unwrap();
    b.transaction(|i| i.set(&product, "name", V::Text("Blue pen".into()), None)).unwrap();
    assert_eq!(b.store.tx, a.store.tx + 1);
    s.stop();
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn a_commit_written_past_the_sequencer_is_noticed() {
    let p = project();
    let url = fresh("foreign");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    // another writer (the command line) commits directly
    let mut direct = engine(&url, m, r, None);
    let c = first(&mut direct, "Customer");
    direct.transaction(|i| i.set(&c, "name", V::Text("Anna".into()), None)).unwrap();
    // the sequencer notices and goes on from the new state (at worst one refusal)
    let mut committed = false;
    for _ in 0..2 {
        let mut a = engine(&url, m, r, Some(&s));
        let pa = first(&mut a, "Product");
        match a.transaction(|i| i.set(&pa, "name", V::Text("Pencil".into()), None)) {
            Ok(()) => {
                committed = true;
                break;
            }
            Err(Flow::Conflict(_)) => continue,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(committed);
    s.stop();
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn the_sequencer_writes_the_same_chain_as_committing_directly() {
    let run = |name: &str, through: bool| -> String {
        let p = project();
        let url = fresh(name);
        init(&p, &url);
        let s = through.then(|| Sequencer::start(&url, Options::default()).unwrap());
        let checked = quadra_analysis::check(&p);
        let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
        for n in 0..10 {
            let mut i = engine(&url, m, r, s.as_ref());
            let customer = first(&mut i, "Customer");
            let product = first(&mut i, "Product");
            i.transaction(|i| {
                let o = i.create("Order", vec![("customer".into(), customer.clone())])?;
                i.create("OrderLine", vec![("order".into(), o), ("product".into(), product.clone()), ("qty".into(), V::Qty { v: (n + 1).into(), unit: "pcs".into() })])?;
                Ok(())
            })
            .unwrap();
        }
        if let Some(s) = s {
            s.stop();
        }
        assert_eq!(verify(&p, &url), Vec::<String>::new());
        let mut c = quadra_store::Conn::open(&url).unwrap();
        c.query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"tx\" DESC LIMIT 1", &[]).unwrap()[0][0].clone().unwrap()
    };
    assert_eq!(run("direct", false), run("through", true));
}

#[test]
fn a_snapshot_and_the_segments_after_it_give_the_database_back() {
    // (snapshots are of SQLite files)
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("segments");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-segments-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let snap = std::env::temp_dir().join(format!("quadra-seq-{}-segments-snap.db", std::process::id()));
    let at = quadra_store::segments::snapshot(&url, &snap).unwrap();
    assert_eq!(at, (0, 1), "the snapshot holds the seed, before any batch");
    let folder = quadra_store::segments::Folder::new(&dir).unwrap();
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    for n in 0..20 {
        let mut i = engine(&url, m, r, Some(&s));
        let customer = first(&mut i, "Customer");
        let product = first(&mut i, "Product");
        i.transaction(|i| {
            let o = i.create("Order", vec![("customer".into(), customer.clone())])?;
            i.create("OrderLine", vec![("order".into(), o), ("product".into(), product.clone()), ("qty".into(), V::Qty { v: (n + 1).into(), unit: "pcs".into() })])?;
            Ok(())
        })
        .unwrap();
    }
    s.stop();
    let segments = folder.segments().unwrap();
    assert!(!segments.is_empty());
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-segments-restored.db", std::process::id()));
    let restored = quadra_store::segments::restore(&snap, &segments, &target).unwrap();
    assert_eq!(restored, 21);
    let restored_url = format!("sqlite:{}", target.display());
    assert_eq!(verify(&p, &restored_url), Vec::<String>::new(), "the restored database checks");
    let last = |u: &str| quadra_store::Conn::open(u).unwrap().query("SELECT \"hash\" FROM \"_quadra_commits\" ORDER BY \"tx\" DESC LIMIT 1", &[]).unwrap()[0][0].clone();
    assert_eq!(last(&url), last(&restored_url), "the same chain");
    assert_eq!(count(&restored_url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"order_line\""), 20);
}

// ---- totals changed by delta -----------------------------------------------------------------

const ACCOUNTS: &str = r#"
entity Account:
    name: Text
    limit: Money<EUR> default 100.00 EUR
    moves: [Move]
    balance = moves.sum(m => m.amount)
    check balance <= limit, "Over the limit"

entity Move:
    account: Account
    amount: Money<EUR>

seed:
    Account(name="Main")
"#;

fn accounts() -> Project {
    accounts_with("")
}

/// The accounts, with more fields on `Account` (indented as its fields).
fn accounts_with(more: &str) -> Project {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    let source = ACCOUNTS.replacen("    check balance <= limit", &format!("{more}    check balance <= limit"), 1);
    Project::from_sources(&root, vec![(root.join("main.qd"), format!("{main}{source}"))])
}

fn eur(s: &str) -> V {
    V::Money { amt: s.parse().unwrap(), cur: "EUR".into() }
}

fn balance(url: &str) -> String {
    let mut c = quadra_store::Conn::open(url).unwrap();
    let cols = c.query("SELECT * FROM \"account\" LIMIT 0", &[]).map(|_| ()).ok();
    let _ = cols;
    c.query("SELECT CAST(\"balance\" AS TEXT) FROM \"account\"", &[]).unwrap()[0][0].clone().unwrap_or_default()
}

#[test]
fn additions_to_the_same_total_from_the_same_snapshot_both_commit() {
    let p = accounts();
    let url = fresh("delta");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    assert!(materialized_formulas(&Database::open(&url, m, r).unwrap().schema).contains(&("Account".into(), "balance".into())), "the balance is kept as a column");
    let mut a = engine(&url, m, r, Some(&s));
    let mut b = engine(&url, m, r, Some(&s));
    let acc_a = first(&mut a, "Account");
    let acc_b = first(&mut b, "Account");
    a.transaction(|i| i.create("Move", vec![("account".into(), acc_a.clone()), ("amount".into(), eur("10.00"))]).map(|_| ())).unwrap();
    // b read the same balance, before a's move: its own move adds up, no conflict
    b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("15.00"))]).map(|_| ())).unwrap();
    s.stop();
    assert_eq!(balance(&url).parse::<rust_decimal::Decimal>().unwrap(), "25".parse().unwrap());
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn a_rule_on_a_total_holds_on_the_latest_value() {
    let p = accounts();
    let url = fresh("guard");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let mut a = engine(&url, m, r, Some(&s));
    let mut b = engine(&url, m, r, Some(&s));
    let acc_a = first(&mut a, "Account");
    let acc_b = first(&mut b, "Account");
    a.transaction(|i| i.create("Move", vec![("account".into(), acc_a.clone()), ("amount".into(), eur("60.00"))]).map(|_| ())).unwrap();
    // alone, b's 60 fits the limit of 100; after a's 60 it does not
    let err = b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("60.00"))]).map(|_| ())).unwrap_err();
    assert!(matches!(err, Flow::Conflict(_)), "{err:?}");
    // run again on the latest data: the rule says why
    let mut b = engine(&url, m, r, Some(&s));
    let acc_b = first(&mut b, "Account");
    let err = b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("60.00"))]).map(|_| ())).unwrap_err();
    assert!(matches!(&err, Flow::Fail(m) if m.contains("Over the limit")), "{err:?}");
    // a move that fits still goes through
    let mut b = engine(&url, m, r, Some(&s));
    let acc_b = first(&mut b, "Account");
    b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("40.00"))]).map(|_| ())).unwrap();
    s.stop();
    assert_eq!(balance(&url).parse::<rust_decimal::Decimal>().unwrap(), "100".parse().unwrap());
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

/// Many transactions at once on a few accounts, each refused or committed by the rules: in the
/// end every rule holds on every record, every total is the sum of what it adds up, and the
/// database verifies. `QUADRA_INVARIANT_TX` sets how many (default 800).
#[test]
fn rules_and_totals_hold_under_concurrent_random_commits() {
    invariants("");
}

/// The same with a formula computed in full from the total: it reads the total, so its value
/// never lags behind the additions of others.
#[test]
fn a_formula_over_a_total_follows_concurrent_additions() {
    invariants("    doubled: Money<EUR> = balance * 2\n");
}

fn invariants(more: &'static str) {
    let p = accounts_with(more);
    let url = fresh(if more.is_empty() { "invariants" } else { "invariants2" });
    init(&p, &url);
    // two more accounts
    {
        let checked = quadra_analysis::check(&p);
        let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
        let mut i = engine(&url, m, r, None);
        i.transaction(|i| {
            i.create("Account", vec![("name".into(), V::Text("Second".into())), ("limit".into(), eur("300.00"))])?;
            i.create("Account", vec![("name".into(), V::Text("Third".into())), ("limit".into(), eur("50.00"))])?;
            Ok(())
        })
        .unwrap();
    }
    let total: usize = std::env::var("QUADRA_INVARIANT_TX").ok().and_then(|v| v.parse().ok()).unwrap_or(800);
    let threads = 8;
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (url, s) = (url.clone(), s.clone());
            std::thread::spawn(move || {
                let p = accounts_with(more);
                let checked = quadra_analysis::check(&p);
                let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
                let mut seed: u64 = 0x2545_f491_4f6c_dd1d ^ (t as u64 + 1);
                let mut next = || {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed
                };
                let (mut committed, mut refused) = (0, 0);
                for _ in 0..total / threads {
                    let which = (next() % 3) as usize;
                    let amount = format!("{}.{:02}", (next() % 80) as i64 - 20, next() % 100);
                    loop {
                        let mut i = engine(&url, m, r, Some(&s));
                        let ids = i.ids("Account", None);
                        let acc = V::Ref { ent: "Account".into(), id: ids[which] };
                        match i.transaction(|i| i.create("Move", vec![("account".into(), acc.clone()), ("amount".into(), eur(&amount))]).map(|_| ())) {
                            Ok(()) => {
                                committed += 1;
                                break;
                            }
                            Err(Flow::Conflict(_)) => continue,
                            Err(Flow::Fail(m)) if m.contains("Over the limit") => {
                                refused += 1;
                                break;
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
                (committed, refused)
            })
        })
        .collect();
    let (mut committed, mut refused) = (0, 0);
    for h in handles {
        let (c, r) = h.join().unwrap();
        committed += c;
        refused += r;
    }
    s.stop();
    eprintln!("{committed} committed, {refused} refused by the limit");
    assert!(committed > 0 && refused > 0, "the test should exercise both outcomes");
    // every total is the sum of its moves, and within its limit
    let mut c = quadra_store::Conn::open(&url).unwrap();
    let moves = c.query("SELECT CAST(\"account_id\" AS TEXT), CAST(\"amount\" AS TEXT) FROM \"move\"", &[]).unwrap();
    let rows = c.query("SELECT CAST(\"id\" AS TEXT), CAST(\"balance\" AS TEXT), CAST(\"limit\" AS TEXT) FROM \"account\"", &[]).unwrap();
    for r in rows {
        let balance: rust_decimal::Decimal = r[1].as_deref().unwrap_or("0").parse().unwrap();
        let limit: rust_decimal::Decimal = r[2].as_deref().unwrap().parse().unwrap();
        let sum: rust_decimal::Decimal = moves.iter().filter(|m| m[0] == r[0]).map(|m| m[1].as_deref().unwrap().parse::<rust_decimal::Decimal>().unwrap()).sum();
        assert_eq!(balance, sum, "account {:?}: the balance is the sum of its moves", r[0]);
        assert!(balance <= limit, "account {:?}: {balance} over the limit {limit}", r[0]);
    }
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn writes_past_the_sequencer_reach_the_log_with_a_snapshot() {
    // (snapshots are of SQLite files)
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("side");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-side-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let mut i = engine(&url, m, r, Some(&s));
    let c = first(&mut i, "Customer");
    i.transaction(|i| i.set(&c, "name", V::Text("Anna B.".into()), None)).unwrap();
    // a team, written as the saved views are: not through the sequencer
    quadra_store::Conn::open(&url)
        .unwrap()
        .exec(
            "INSERT INTO \"_quadra_teams\" (\"id\", \"name\", \"description\", \"version\", \"created_at\", \"updated_at\") VALUES (1, 'Sales', '', 1, 'x', 'x')",
            &[],
        )
        .unwrap();
    s.snapshot_soon(std::time::Duration::ZERO);
    // another commit after it: the folder holds the snapshot, then this batch
    let mut i = engine(&url, m, r, Some(&s));
    let c = first(&mut i, "Customer");
    i.transaction(|i| i.set(&c, "name", V::Text("Anna Bianchi".into()), None)).unwrap();
    s.stop();
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-side-rebuilt.db", std::process::id()));
    quadra_store::segments::Folder::new(&dir).unwrap().restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    assert_eq!(count(&t, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_teams\""), 1, "the team is in what the folder rebuilds");
    assert_eq!(count(&t, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_commits\""), count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_commits\""));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
}

#[test]
fn a_formula_over_a_total_never_keeps_a_stale_value() {
    let p = accounts_with("    doubled: Money<EUR> = balance * 2\n");
    let url = fresh("doubled");
    init(&p, &url);
    let s = Sequencer::start(&url, Options::default()).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let mut a = engine(&url, m, r, Some(&s));
    let mut b = engine(&url, m, r, Some(&s));
    let acc_a = first(&mut a, "Account");
    let acc_b = first(&mut b, "Account");
    a.transaction(|i| i.create("Move", vec![("account".into(), acc_a.clone()), ("amount".into(), eur("10.00"))]).map(|_| ())).unwrap();
    // b computed `doubled` from the balance it read, before a's move: refused, or right
    match b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("15.00"))]).map(|_| ())) {
        Ok(()) => {}
        Err(Flow::Conflict(_)) => {
            let mut b = engine(&url, m, r, Some(&s));
            let acc_b = first(&mut b, "Account");
            b.transaction(|i| i.create("Move", vec![("account".into(), acc_b.clone()), ("amount".into(), eur("15.00"))]).map(|_| ())).unwrap();
        }
        Err(e) => panic!("{e:?}"),
    }
    s.stop();
    let mut c = quadra_store::Conn::open(&url).unwrap();
    let row = c.query("SELECT CAST(\"balance\" AS TEXT), CAST(\"doubled\" AS TEXT) FROM \"account\"", &[]).unwrap();
    let balance: rust_decimal::Decimal = row[0][0].as_deref().unwrap().parse().unwrap();
    let doubled: rust_decimal::Decimal = row[0][1].as_deref().unwrap().parse().unwrap();
    assert_eq!(balance, "25".parse().unwrap());
    assert_eq!(doubled, "50".parse().unwrap(), "the formula follows both additions");
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

#[test]
fn a_database_behind_its_log_is_brought_back_on_start() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("behind");
    let file = url.strip_prefix("sqlite:").unwrap().to_string();
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-behind-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let options = || Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options()).unwrap();
    let commit = |s: &Sequencer, name: &str| {
        let mut i = engine(&url, m, r, Some(s));
        let c = first(&mut i, "Customer");
        i.transaction(|i| i.set(&c, "name", V::Text(name.into()), None)).unwrap();
    };
    commit(&s, "one");
    s.stop();
    // the machine keeps the database as it was here (a power cut loses what follows)
    let old = format!("{file}.old");
    quadra_store::segments::snapshot(&url, Path::new(&old)).unwrap();
    let s = Sequencer::start(&url, options()).unwrap();
    commit(&s, "two");
    commit(&s, "three");
    s.stop();
    let head = count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\"");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{file}{suffix}"));
    }
    std::fs::rename(&old, &file).unwrap();
    assert!(count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\"") < head, "the database lost its last commits");
    // the sequencer starts: the database gets them back from the folder
    let s = Sequencer::start(&url, options()).unwrap();
    s.stop();
    assert_eq!(count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), head);
    let mut c = quadra_store::Conn::open(&url).unwrap();
    assert_eq!(c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap()[0][0].as_deref(), Some("three"));
    assert_eq!(verify(&p, &url), Vec::<String>::new());
}

/// A folder that refuses every batch while `down` is set.
struct Flaky {
    folder: quadra_store::segments::Folder,
    down: Arc<std::sync::atomic::AtomicBool>,
}

impl DurabilitySink for Flaky {
    fn persist(&mut self, b: &Batch) -> Result<(), String> {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("unreachable".into());
        }
        self.folder.persist(b)
    }
    fn last(&mut self) -> Result<Option<(u64, u64)>, String> {
        self.folder.last()
    }
    fn segments_after(&mut self, batch: u64) -> Result<Vec<quadra_store::segments::Segment>, String> {
        self.folder.segments_after(batch)
    }
    fn snapshot(&mut self, batch: u64, tx: u64, file: &Path) -> Result<(), String> {
        self.folder.snapshot(batch, tx, file)
    }
}

#[test]
fn a_destination_down_for_a_while_gets_every_batch_in_order() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("flaky");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-flaky-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flaky = Flaky { folder: quadra_store::segments::Folder::new(&dir).unwrap(), down: down.clone() };
    let s = Sequencer::start(&url, Options { destinations: vec![Box::new(flaky)], rule: Rule::All, max_behind: 3, ..Default::default() }).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let commit = |name: &str| {
        let mut i = engine(&url, m, r, Some(&s));
        let c = first(&mut i, "Customer");
        i.transaction(|i| i.set(&c, "name", V::Text(name.into()), None)).map(|_| ())
    };
    commit("one").unwrap();
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    // in the database, not kept elsewhere: the client hears it failed
    assert!(commit("two").is_err());
    // more of them while it is down: past `max_behind` commits not kept, they are refused
    // before being written
    let commits = || count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_commits\"");
    let before = commits();
    for n in 0..10 {
        assert!(commit(&format!("more {n}")).is_err());
    }
    assert!(commits() - before <= 3, "{} commits written while the destination was down", commits() - before);
    // what the host's monitoring sees meanwhile
    let h = s.health().unwrap();
    assert!(h.destinations && h.down.is_some() && h.refused > 0 && h.kept < h.written, "{h:?}");
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    // back: what was written goes out in order, and commits are confirmed again
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while commit("back").is_err() {
        assert!(std::time::Instant::now() < until, "commits are still refused once the destination is back");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let h = s.health().unwrap();
    assert!(h.down.is_none() && h.kept == h.written, "{h:?}");
    s.stop();
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-flaky-rebuilt.db", std::process::id()));
    quadra_store::segments::Folder::new(&dir).unwrap().restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    assert_eq!(count(&t, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""));
    let mut c = quadra_store::Conn::open(&t).unwrap();
    assert_eq!(c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap()[0][0].as_deref(), Some("back"));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
}

#[test]
fn the_log_is_compacted_by_its_snapshots() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("compact");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-compact-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // a snapshot as soon as the log outgrows the database
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, snapshot_bytes: 0, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    for n in 0..300 {
        let mut i = engine(&url, m, r, Some(&s));
        let c = first(&mut i, "Customer");
        let long = format!("{n} {}", "x".repeat(2000));
        i.transaction(|i| i.set(&c, "name", V::Text(long.as_str().into()), None)).unwrap();
    }
    s.stop();
    // one snapshot left, taken along the way, and only the segments after it
    let folder = quadra_store::segments::Folder::new(&dir).unwrap();
    let (batch, at, _) = folder.latest_snapshot().unwrap().unwrap();
    assert!(at > 100, "no snapshot along the way (the latest is at {at})");
    let snapshots = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".snapshot.db")).count();
    assert_eq!(snapshots, 1);
    assert!(folder.segments().unwrap().iter().all(|s| s.first > batch && s.first_tx > at));
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-compact-rebuilt.db", std::process::id()));
    quadra_store::segments::Folder::new(&dir).unwrap().restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    assert_eq!(count(&t, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
}

#[test]
fn the_log_gives_the_database_back_as_of_any_transaction() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("pitr");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-pitr-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    // several threads at once: batches of several transactions, cut inside
    let threads: Vec<_> = (0..6)
        .map(|t| {
            let (url, s) = (url.clone(), s.clone());
            std::thread::spawn(move || {
                let p = project();
                let checked = quadra_analysis::check(&p);
                let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
                for n in 0..20 {
                    let name = V::Text(format!("customer {t}-{n}").into());
                    for _ in 0..50 {
                        let mut i = engine(&url, m, r, Some(&s));
                        match i.transaction(|i| i.create("Customer", vec![("name".into(), name.clone())]).map(|_| ())) {
                            Ok(()) => break,
                            Err(Flow::Conflict(_)) => {}
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    s.stop();
    let batches = count(&url, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_batches\"");
    assert!(batches < 120, "every batch held one transaction: no cut inside a batch is tested");
    let folder = quadra_store::segments::Folder::new(&dir).unwrap();
    let hash = |url: &str, tx: i64| {
        let mut c = quadra_store::Conn::open(url).unwrap();
        c.query(&format!("SELECT \"hash\" FROM \"_quadra_commits\" WHERE \"tx\" = {tx}"), &[]).unwrap()[0][0].clone()
    };
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-pitr.db", std::process::id()));
    let t = format!("sqlite:{}", target.display());
    // every transaction after the snapshot the log starts from (the seed, 1)
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..25 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let tx = 2 + seed % 120;
        assert_eq!(folder.restore_to(&target, Some(tx)).unwrap(), tx);
        assert_eq!(count(&t, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), tx as i64);
        assert_eq!(hash(&t, tx as i64), hash(&url, tx as i64), "the chain as of {tx}");
        // one customer per transaction
        assert_eq!(count(&t, "SELECT CAST(COUNT(*) AS TEXT) FROM \"customer\""), tx as i64);
        assert_eq!(verify(&p, &t), Vec::<String>::new(), "as of {tx}");
    }
}

#[test]
fn changes_outside_the_chain_are_in_the_log_in_order() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("outside");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-side2-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let commit = |name: &str| {
        let mut i = engine(&url, m, r, Some(&s));
        let c = first(&mut i, "Customer");
        i.transaction(|i| i.set(&c, "name", V::Text(name.into()), None)).unwrap();
    };
    let session = |name: &'static str| s.side(move |c| quadra_store::auth::create(c, "session", 1, name, false, clock(), None)).unwrap();
    commit("one");
    let (first_secret, _) = session("first");
    // a change that gives up writes nothing
    assert!(s.side(|c| -> Result<(), String> {
        c.exec("UPDATE \"_quadra_tokens\" SET \"name\" = 'x'", &[])?;
        Err("given up".into())
    })
    .is_err());
    commit("two");
    s.snapshot_soon(std::time::Duration::ZERO);
    session("second");
    session("third");
    commit("three");
    s.stop();
    let tokens = |u: &str| count(u, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_tokens\"");
    assert_eq!(tokens(&url), 3);
    // the machine is lost: the folder alone gives back the sessions with the data
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-side2-rebuilt.db", std::process::id()));
    quadra_store::segments::Folder::new(&dir).unwrap().restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    assert_eq!(tokens(&t), 3, "the sessions are in what the log rebuilds");
    assert_eq!(count(&t, "SELECT CAST(COUNT(*) AS TEXT) FROM \"_quadra_tokens\" WHERE \"name\" = 'x'"), 0);
    let mut c = quadra_store::Conn::open(&t).unwrap();
    assert!(quadra_store::auth::authenticate(&mut c, &first_secret, clock()).unwrap().is_some(), "a session signed in before the loss still works");
    assert_eq!(c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap()[0][0].as_deref(), Some("three"));
    assert_eq!(count(&t, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
}

#[test]
fn a_number_given_up_leaves_no_gap_in_the_log() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("givenup");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-givenup-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options { destinations: vec![Box::new(quadra_store::segments::Folder::new(&dir).unwrap())], rule: Rule::All, ..Default::default() };
    let s = Sequencer::start(&url, options).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let commit = |name: &str| {
        let mut i = engine(&url, m, r, Some(&s));
        let c = first(&mut i, "Customer");
        i.transaction(|i| i.set(&c, "name", V::Text(name.into()), None)).unwrap();
    };
    commit("one");
    // a transaction numbered, then given up (its writes could not be prepared)
    let head = count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\"") as u64;
    let reserved = s
        .reserve(quadra_store::sequencer::Request { base: head, whole: false, reads: Default::default(), writes: Default::default(), guards: Vec::new(), history: 0, outbox: 0, at: "2026-12-01T10:00:00".into(), user: None, cause: "test".into() })
        .unwrap();
    s.cancel(reserved.tx);
    commit("two");
    commit("three");
    s.stop();
    // the log rebuilds the database across the number given up, and to any transaction
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-givenup-rebuilt.db", std::process::id()));
    let folder = quadra_store::segments::Folder::new(&dir).unwrap();
    folder.restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    assert_eq!(count(&t, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""), count(&url, "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\""));
    let mut c = quadra_store::Conn::open(&t).unwrap();
    assert_eq!(c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap()[0][0].as_deref(), Some("three"));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
    // as of the number given up: the commit before it
    assert_eq!(folder.restore_to(&target, Some(reserved.tx)).unwrap(), reserved.tx - 1);
}

#[test]
fn under_the_local_rule_a_destination_down_still_gets_every_batch() {
    if std::env::var_os("QUADRA_SEQ_PG").is_some() {
        return;
    }
    let p = project();
    let url = fresh("localdown");
    init(&p, &url);
    let dir = std::env::temp_dir().join(format!("quadra-seq-{}-localdown-log", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flaky = Flaky { folder: quadra_store::segments::Folder::new(&dir).unwrap(), down: down.clone() };
    let s = Sequencer::start(&url, Options { destinations: vec![Box::new(flaky)], rule: Rule::Local, ..Default::default() }).unwrap();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let commit = |name: &str| {
        let mut i = engine(&url, m, r, Some(&s));
        let c = first(&mut i, "Customer");
        i.transaction(|i| i.set(&c, "name", V::Text(name.into()), None)).map(|_| ())
    };
    commit("one").unwrap();
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    // confirmed at once (the local rule), kept later
    for n in 0..5 {
        commit(&format!("while down {n}")).unwrap();
    }
    let h = s.health().unwrap();
    assert!(h.kept < h.written, "what is not kept shows: {h:?}");
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    commit("back").unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while s.health().unwrap().kept < s.health().unwrap().written {
        assert!(std::time::Instant::now() < until, "the batches written while it was down never went");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    s.stop();
    let target = std::env::temp_dir().join(format!("quadra-seq-{}-localdown-rebuilt.db", std::process::id()));
    quadra_store::segments::Folder::new(&dir).unwrap().restore(&target).unwrap();
    let t = format!("sqlite:{}", target.display());
    let mut c = quadra_store::Conn::open(&t).unwrap();
    assert_eq!(c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap()[0][0].as_deref(), Some("back"));
    assert_eq!(verify(&p, &t), Vec::<String>::new());
}
