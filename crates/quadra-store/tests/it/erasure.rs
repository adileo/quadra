//! Erasure through the sequencer: the values erased leave the database, the log and its
//! snapshot, the chain still verifies, and a copy of an earlier moment rebuilt from the log
//! does not show them either.

use quadra_runtime::{Ctx, Interp, V};
use quadra_sema::Project;
use quadra_store::segments::{Folder, Segment};
use quadra_store::sequencer::{Batch, DurabilitySink, Options, Rule};
use quadra_store::{materialized_formulas, Database, DbSource, SequencedSink, Sequencer, Sink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MODEL: &str = r#"
entity Customer:
    name: Text? personal
    email: Email? personal
    city: Text?

entity Visit:
    customer: Customer parent
    note: Text? personal

seed:
    let a = Customer(name="Anna Bianchi", email="anna@example.com", city="Milano")
    let z = Customer(name="Zeno Zanetti", email="zeno.zanetti@example.com", city="Torino")
    Visit(customer=z, note="Zeno asked for a call")
"#;

fn project(dir: &Path) -> Project {
    std::fs::create_dir_all(dir).unwrap();
    let main = dir.join("main.qd");
    Project::from_sources(dir, vec![(main, MODEL.to_string())])
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quadra-erasure-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 12, 1)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

/// A destination keeping every batch and its first snapshot, and never pruning, as a backup of
/// the log would: the copy of an earlier moment is rebuilt from it.
#[derive(Clone)]
struct KeepAll(Arc<Mutex<(Vec<Vec<u8>>, Option<(u64, PathBuf)>)>>, PathBuf);

impl DurabilitySink for KeepAll {
    fn persist(&mut self, b: &Batch) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .0
            .push(quadra_store::segments::encode(b));
        Ok(())
    }

    fn snapshot(&mut self, batch: u64, _tx: u64, file: &Path) -> Result<(), String> {
        let mut g = self.0.lock().unwrap();
        if g.1.is_none() {
            let to = self.1.join("first.snapshot.db");
            std::fs::copy(file, &to).map_err(|e| e.to_string())?;
            g.1 = Some((batch, to));
        }
        Ok(())
    }
}

fn engine<'m>(
    url: &str,
    model: &'m quadra_ir::Model,
    report: &'m quadra_types::Report,
    s: &Sequencer,
) -> Interp<'m> {
    let db = Database::open(url, model, report).unwrap();
    let src = DbSource::open(url, db.schema.clone(), None).unwrap();
    let mut i = Interp::new(model);
    i.set_clock(clock());
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(SequencedSink {
        db,
        model,
        sequencer: s.clone(),
        tickets: None,
    }));
    i.load_units();
    i
}

fn contains(path: &Path, text: &str) -> bool {
    let mut all = Vec::new();
    for suffix in ["", "-wal"] {
        if let Ok(b) = std::fs::read(format!("{}{suffix}", path.display())) {
            all.extend(b);
        }
    }
    all.windows(text.len()).any(|w| w == text.as_bytes())
}

fn dump(url: &str) -> String {
    let mut c = quadra_store::Conn::open(url).unwrap();
    let mut out = String::new();
    let tables = c
        .query("SELECT name FROM sqlite_master WHERE type = 'table'", &[])
        .unwrap();
    for t in tables {
        let t = t[0].clone().unwrap();
        for r in c.query(&format!("SELECT * FROM \"{t}\""), &[]).unwrap() {
            out.push_str(&format!("{t}: {r:?}\n"));
        }
    }
    out
}

#[test]
fn an_erasure_leaves_the_database_the_log_and_the_past() {
    quadra_store::db::seed_all_salts();
    let dir = tmp("all");
    let p = project(&dir.join("project"));
    let checked = quadra_analysis::check(&p);
    assert!(checked.model.is_some(), "{:?}", checked.diagnostics);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let file = dir.join("data.db");
    let url = format!("sqlite:{}", file.display());
    {
        let mut db = Database::open(&url, m, r).unwrap();
        db.init().unwrap();
        let mut i = Interp::new(m);
        i.set_clock(clock());
        i.materialize = materialized_formulas(&db.schema);
        i.sink = Some(Box::new(Sink { db, model: m }));
        i.system += 1;
        let seeds = m.seeds.clone();
        i.transaction(|i| {
            for s in &seeds {
                i.stmts(&s.body, &mut Ctx::default())?;
            }
            Ok(())
        })
        .unwrap();
    }
    let log = dir.join("log");
    let kept = KeepAll(Arc::new(Mutex::new((Vec::new(), None))), dir.clone());
    let options = Options {
        destinations: vec![Box::new(Folder::new(&log).unwrap()), Box::new(kept.clone())],
        rule: Rule::All,
        ..Default::default()
    };
    let s = Sequencer::start(&url, options).unwrap();
    // a change before the erasure: the moment a copy is made of later
    let mut i = engine(&url, m, r, &s);
    let zeno = i
        .ids("Customer", None)
        .into_iter()
        .find(|id| i.store.read(*id, "name", None) == Some(V::Text("Zeno Zanetti".into())))
        .unwrap();
    let z = V::Ref {
        ent: "Customer".into(),
        id: zeno,
    };
    i.transaction(|i| i.set(&z, "city", V::Text("Genova".into()), None))
        .unwrap();
    let before = i.store.tx;
    drop(i);
    let mut i = engine(&url, m, r, &s);
    let erased = i
        .with_cause("erase test".into(), |i| {
            i.transaction(|i| i.erase(&z, "he asked on 2026-12-01"))
        })
        .unwrap();
    assert_eq!(
        erased,
        vec!["Customer.name", "Customer.email", "Visit.note"]
    );
    drop(i);
    // the snapshot that follows an erasure: waited for, then the sequencer stops
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let f = Folder::new(&log).unwrap();
        if f.latest_snapshot()
            .unwrap()
            .is_some_and(|(_, t, _)| t > before)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no snapshot after the erasure"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    s.stop();

    let secrets = [
        "Zeno Zanetti",
        "zeno.zanetti@example.com",
        "asked for a call",
    ];
    // the database: its file as it is (no `VACUUM` here: `secure_delete` overwrote what was
    // erased, and the sequencer emptied the WAL that held the pages as they were)
    let d = dump(&url);
    for t in secrets {
        assert!(!d.contains(t), "the database still holds {t}");
        assert!(!contains(&file, t), "the database file still holds {t}");
    }
    assert!(d.contains("Genova"), "a value that is not personal stays");
    // the log: one snapshot after the erasure, and only the segments after it
    let folder = Folder::new(&log).unwrap();
    for e in std::fs::read_dir(&log).unwrap().flatten() {
        let bytes = std::fs::read(e.path()).unwrap();
        for t in secrets {
            assert!(
                !bytes.windows(t.len()).any(|w| w == t.as_bytes()),
                "{} still holds {t}",
                e.path().display()
            );
        }
    }
    // the chain verifies, with the digests of what was erased
    let checked2 = quadra_analysis::check(&p);
    let (m2, r2) = (checked2.model.as_ref().unwrap(), &checked2.report);
    let (problems, _) = quadra_store::ops::verify(&url, m2, r2, clock()).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let (n, _, _) = quadra_store::verify::facts(&url).unwrap();
    assert!(n >= 3, "{n} values erased");
    // what the log rebuilds verifies too, and holds none of it
    let rebuilt = dir.join("rebuilt.db");
    folder.restore(&rebuilt).unwrap();
    let rurl = format!("sqlite:{}", rebuilt.display());
    assert_eq!(
        quadra_store::ops::verify(&rurl, m2, r2, clock()).unwrap().0,
        Vec::<String>::new()
    );
    // a copy of the moment before the erasure, from a backup of the log that kept every batch:
    // the erasures that came after are applied to it
    let (bytes, snap) = kept.0.lock().unwrap().clone();
    let (snap_batch, snap) = snap.expect("the backup got a snapshot when the sequencer started");
    let segments: Vec<Segment> = bytes
        .iter()
        .map(|b| quadra_store::segments::decode(b).unwrap())
        .filter(|s| s.number > snap_batch)
        .collect();
    let past = dir.join("past.db");
    let at = quadra_store::segments::restore_until(&snap, &segments, &past, Some(before)).unwrap();
    assert_eq!(at, before);
    let pc = format!("sqlite:{}", past.display());
    let pd = dump(&pc);
    for t in secrets {
        assert!(!pd.contains(t), "the copy of the past still holds {t}");
    }
    assert!(
        pd.contains("Genova"),
        "the copy is of the moment after the change of city"
    );
    assert_eq!(
        quadra_store::ops::verify(&pc, m2, r2, clock()).unwrap().0,
        Vec::<String>::new()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What is deleted gives its space back: a file that grew with 20,000 rows goes back to about
/// its first size once they are deleted and `reclaim` runs; a file made before `auto_vacuum`
/// is turned into one with it.
#[test]
fn deleted_rows_give_their_space_back() {
    let dir = std::env::temp_dir().join(format!("quadra-reclaim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    for old in [false, true] {
        let file = dir.join(format!("r{old}.db"));
        if old {
            // as a file of an earlier version: no auto_vacuum
            let c = rusqlite::Connection::open(&file).unwrap();
            c.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        }
        let url = format!("sqlite:{}", file.display());
        let mut c = quadra_store::Conn::open(&url).unwrap();
        c.batch("CREATE TABLE IF NOT EXISTS t (v TEXT)").unwrap();
        quadra_store::conn::reclaim(&mut c).unwrap();
        let empty = size(&file);
        c.batch("BEGIN").unwrap();
        for i in 0..20_000 {
            c.batch(&format!(
                "INSERT INTO t VALUES ('row {i} {}')",
                "x".repeat(200)
            ))
            .unwrap();
        }
        c.batch("COMMIT").unwrap();
        quadra_store::conn::reclaim(&mut c).unwrap();
        let full = size(&file);
        c.batch("DELETE FROM t").unwrap();
        assert!(quadra_store::conn::reclaim(&mut c).unwrap());
        let after = size(&file);
        assert!(full > 4_000_000, "{full}");
        assert!(
            after < empty + 64 * 1024,
            "old {old}: {empty} → {full} → {after}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
