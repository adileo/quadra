//! Crashes: a process commits through the sequencer, keeping every batch in a folder before it
//! confirms, and notes each confirmed commit; it is killed (`kill -9`) at random moments, again
//! and again. Afterwards every confirmed commit is in the database and in what the folder
//! rebuilds, and both verify.
//!
//! `QUADRA_CRASHES` sets how many kills (default 6; the gate asks for 1 000).

use quadra_runtime::{Ctx, Interp, V};
use quadra_sema::Project;
use quadra_store::sequencer::{Options, Rule};
use quadra_store::{materialized_formulas, Database, DbSource, SequencedSink, Sequencer, Sink};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SEED: &str = r#"
seed:
    Customer(name="Anna Bianchi")
"#;

fn project() -> Project {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let main = std::fs::read_to_string(root.join("main.qd")).unwrap();
    Project::from_sources(&root, vec![(root.join("main.qd"), format!("{main}{SEED}"))])
}

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("quadra-crash-{}", std::env::var("QUADRA_CRASH_ID").unwrap_or_else(|_| std::process::id().to_string())))
}

fn url() -> String {
    format!("sqlite:{}", dir().join("data.db").display())
}

fn init(project: &Project, url: &str) {
    let checked = quadra_analysis::check(project);
    let model = checked.model.as_ref().unwrap();
    let mut db = Database::open(url, model, &checked.report).unwrap();
    db.init().unwrap();
    let mut i = Interp::new(model);
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model }));
    i.system += 1;
    let seeds = model.seeds.clone();
    i.transaction(|i| {
        for s in &seeds {
            i.stmts(&s.body, &mut Ctx::default())?;
        }
        Ok(())
    })
    .unwrap();
}

/// The writer, in a process of its own: commits until it is killed.
#[test]
#[ignore]
fn crash_child() {
    if std::env::var_os("QUADRA_CRASH_ID").is_none() {
        return;
    }
    let p = project();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let url = url();
    let folder = quadra_store::segments::Folder::new(dir().join("log")).unwrap();
    let s = Sequencer::start(&url, Options { destinations: vec![Box::new(folder)], rule: Rule::All, ..Default::default() }).unwrap();
    let run = std::env::var("QUADRA_CRASH_RUN").unwrap();
    let mut acked = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("acked.txt")).unwrap();
    for n in 0.. {
        let db = Database::open(&url, m, r).unwrap();
        let src = DbSource::open(&url, db.schema.clone(), None).unwrap();
        let mut i = Interp::new(m);
        i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
        i.materialize = materialized_formulas(&db.schema);
        i.sink = Some(Box::new(SequencedSink { db, model: m, sequencer: s.clone(), tickets: None }));
        i.load_units();
        let name = format!("c-{run}-{n}");
        i.transaction(|i| i.create("Customer", vec![("name".into(), V::Text(name.clone().into()))]).map(|_| ())).unwrap();
        // confirmed: noted, on disk, before the next one (one write: a kill never splits a line)
        acked.write_all(format!("{name}\n").as_bytes()).unwrap();
        acked.sync_data().unwrap();
    }
}

fn names(url: &str) -> std::collections::HashSet<String> {
    let mut c = quadra_store::Conn::open(url).unwrap();
    c.query("SELECT \"name\" FROM \"customer\"", &[]).unwrap().into_iter().filter_map(|r| r[0].clone()).collect()
}

fn verify(url: &str) -> Vec<String> {
    let p = project();
    let checked = quadra_analysis::check(&p);
    let now = chrono::NaiveDate::from_ymd_opt(2026, 12, 1).unwrap().and_hms_opt(10, 0, 0).unwrap();
    quadra_store::ops::verify(url, checked.model.as_ref().unwrap(), &checked.report, now).unwrap().0
}

#[test]
fn confirmed_commits_survive_being_killed_at_random() {
    let id = format!("{}", std::process::id());
    std::env::set_var("QUADRA_CRASH_ID", &id);
    let _ = std::fs::remove_dir_all(dir());
    std::fs::create_dir_all(dir()).unwrap();
    let p = project();
    init(&p, &url());
    let crashes: usize = std::env::var("QUADRA_CRASHES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let exe = std::env::current_exe().unwrap();
    // a small pseudo-random sequence of moments: the same on every run of the test
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let started = Instant::now();
    for run in 0..crashes {
        let mut child = Command::new(&exe)
            .args(["crash_child", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env("QUADRA_CRASH_ID", &id)
            .env("QUADRA_CRASH_RUN", run.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300 + next() % 900));
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let acked: Vec<String> = std::fs::read_to_string(dir().join("acked.txt")).unwrap_or_default().lines().map(str::to_string).collect();
    eprintln!("{crashes} kills in {:?}, {} commits confirmed", started.elapsed(), acked.len());
    assert!(!acked.is_empty(), "nothing was ever confirmed: the writer did not run");

    // the database holds every confirmed commit, and verifies
    let in_db = names(&url());
    let lost: Vec<&String> = acked.iter().filter(|n| !in_db.contains(*n)).collect();
    assert!(lost.is_empty(), "confirmed and lost from the database: {lost:?}");
    assert_eq!(verify(&url()), Vec::<String>::new());

    // so does the database rebuilt from the folder alone
    let rebuilt = dir().join("rebuilt.db");
    quadra_store::segments::Folder::new(dir().join("log")).unwrap().restore(&rebuilt).unwrap();
    let rebuilt_url = format!("sqlite:{}", rebuilt.display());
    let in_log = names(&rebuilt_url);
    let lost: Vec<&String> = acked.iter().filter(|n| !in_log.contains(*n)).collect();
    assert!(lost.is_empty(), "confirmed and missing from what the folder rebuilds: {lost:?}");
    assert_eq!(verify(&rebuilt_url), Vec::<String>::new());
}
