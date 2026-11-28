//! Databases that grow: `history 1 year` drops the values replaced more than a year ago, and
//! `archive` moves the values replaced before a date to an archive; the tables stay the same,
//! the database is smaller, `verify` and `verify --full` pass, and an archive altered is found
//! out.

use quadra_runtime::{Interp, V};
use quadra_sema::Project;
use quadra_store::{materialized_formulas, ops, Conn, Database, DbSource, Sink};
use std::path::{Path, PathBuf};

const MODEL: &str = r#"
entity Sensor:
    history 1 year
    name: Text
    reading: Decimal

entity Note:
    text: Text

seed:
    Sensor(name="boiler", reading=10)
    Note(text="first")
"#;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quadra-archive-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

fn engine<'m>(
    url: &str,
    m: &'m quadra_ir::Model,
    r: &'m quadra_types::Report,
    time: chrono::NaiveDateTime,
) -> Interp<'m> {
    let db = Database::open(url, m, r).unwrap();
    let src = DbSource::open(url, db.schema.clone(), None).unwrap();
    let mut i = Interp::new(m);
    i.set_clock(time);
    i.store = quadra_runtime::Store::over(std::rc::Rc::new(std::cell::RefCell::new(src)));
    i.materialize = materialized_formulas(&db.schema);
    i.sink = Some(Box::new(Sink { db, model: m }));
    i.load_units();
    i
}

fn set(
    url: &str,
    m: &quadra_ir::Model,
    r: &quadra_types::Report,
    time: chrono::NaiveDateTime,
    ent: &str,
    field: &str,
    v: V,
) {
    let mut i = engine(url, m, r, time);
    let rec = V::Ref {
        ent: ent.into(),
        id: i.ids(ent, None)[0],
    };
    i.transaction(|i| i.set(&rec, field, v, None)).unwrap();
}

fn tables(url: &str) -> Vec<String> {
    let mut c = Conn::open(url).unwrap();
    let mut out = Vec::new();
    for t in ["sensor", "note"] {
        for r in c
            .query(&format!("SELECT * FROM \"{t}\" ORDER BY \"id\""), &[])
            .unwrap()
        {
            out.push(format!("{t}: {r:?}"));
        }
    }
    out
}

fn history_rows(url: &str) -> usize {
    Conn::open(url)
        .unwrap()
        .query("SELECT \"seq\" FROM \"_quadra_history\"", &[])
        .unwrap()
        .len()
}

fn setup(dir: &Path) -> (Project, String) {
    let pdir = dir.join("project");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join("quadra.toml"), "[project]\nname = \"a\"\n").unwrap();
    std::fs::write(pdir.join("main.qd"), MODEL).unwrap();
    let p = Project::load(&pdir).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    (p, url)
}

#[test]
fn history_drops_the_values_replaced_more_than_a_year_ago() {
    quadra_store::db::seed_all_salts();
    let dir = tmp("history");
    let (p, url) = setup(&dir);
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, at(2024, 1, 1)).unwrap();
    set(
        &url,
        m,
        r,
        at(2024, 6, 1),
        "Note",
        "text",
        V::Text("second".into()),
    );
    set(
        &url,
        m,
        r,
        at(2024, 6, 1),
        "Sensor",
        "reading",
        V::Dec(20.into()),
    );
    set(
        &url,
        m,
        r,
        at(2025, 3, 1),
        "Sensor",
        "reading",
        V::Dec(30.into()),
    );
    set(
        &url,
        m,
        r,
        at(2026, 11, 1),
        "Sensor",
        "reading",
        V::Dec(40.into()),
    );
    let before = tables(&url);
    ops::sweep(&url, m, r, at(2026, 12, 1)).unwrap();
    let mut c = Conn::open(&url).unwrap();
    let readings: Vec<Option<String>> = c
        .query("SELECT h.\"value\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE r.\"entity\" = 'Sensor' AND h.\"field\" = 'reading' ORDER BY h.\"seq\"", &[])
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    // 10 (replaced in June 2024) and 20 (replaced in March 2025) go; 30 was in force a year
    // ago, 40 is now
    assert_eq!(readings.len(), 4);
    assert_eq!(readings[0], None);
    assert_eq!(readings[1], None);
    assert!(
        readings[2].as_deref().is_some_and(|v| v.contains("30")),
        "{readings:?}"
    );
    assert!(
        readings[3].as_deref().is_some_and(|v| v.contains("40")),
        "{readings:?}"
    );
    // a note has no `history`: all of it stays
    let notes = c
        .query("SELECT h.\"value\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE r.\"entity\" = 'Note' AND h.\"value\" IS NULL", &[])
        .unwrap();
    assert!(notes.is_empty());
    drop(c);
    assert_eq!(tables(&url), before, "the tables do not change");
    assert_eq!(
        ops::verify(&url, m, r, at(2026, 12, 1)).unwrap().0,
        Vec::<String>::new()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_archive_leaves_the_tables_and_is_checked_by_its_checkpoint() {
    quadra_store::db::seed_all_salts();
    let dir = tmp("archive");
    let (p, url) = setup(&dir);
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, at(2024, 1, 1)).unwrap();
    for (n, d) in (1..=200).zip((0..200).map(|k| at(2024, 2, 1) + chrono::Duration::days(k))) {
        let long = format!("note {n} {}", "x".repeat(2000));
        set(&url, m, r, d, "Note", "text", V::Text(long.into()));
    }
    set(
        &url,
        m,
        r,
        at(2026, 6, 1),
        "Note",
        "text",
        V::Text("last".into()),
    );
    let before = tables(&url);
    let rows = history_rows(&url);
    let size = |url: &str| {
        let mut c = Conn::open(url).unwrap();
        c.batch("VACUUM").unwrap();
        c.batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        std::fs::metadata(url.trim_start_matches("sqlite:"))
            .unwrap()
            .len()
    };
    let size_before = size(&url);
    let mut kept: Vec<(String, Vec<u8>)> = Vec::new();
    let c =
        quadra_store::archive::archive(&url, m, r, at(2025, 1, 1), at(2026, 12, 1), &mut |k, b| {
            kept.push((k.to_string(), b.to_vec()));
            Ok(())
        })
        .unwrap()
        .expect("values to archive");
    assert_eq!(kept.len(), 1);
    assert!(c.rows >= 190, "{} rows", c.rows);
    assert_eq!(history_rows(&url), rows - c.rows as usize);
    let size_after = size(&url);
    assert!(
        size_after + 300_000 < size_before,
        "{size_before} then {size_after}"
    );
    assert_eq!(tables(&url), before, "the tables do not change");
    // verify: the commits before the checkpoint by their links and signatures; --full: all
    assert_eq!(
        ops::verify(&url, m, r, at(2026, 12, 1)).unwrap().0,
        Vec::<String>::new()
    );
    let rows_back = quadra_store::archive::read(&c, &kept[0].1).unwrap();
    assert_eq!(
        quadra_store::verify::verify_with(&url, m, r, at(2026, 12, 1), Some(rows_back.clone()))
            .unwrap()
            .0,
        Vec::<String>::new()
    );
    // a value altered in the archive: refused when read back
    let mut text = String::new();
    use std::io::{Read, Write};
    flate2::read::GzDecoder::new(&kept[0].1[..])
        .read_to_string(&mut text)
        .unwrap();
    let altered = text.replacen("note 7 ", "note 8 ", 1);
    assert_ne!(altered, text);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(altered.as_bytes()).unwrap();
    let e = quadra_store::archive::read(&c, &gz.finish().unwrap()).unwrap_err();
    assert!(e.contains("the archive was altered"), "{e}");
    // rows left out of the archive: --full finds the commits that do not compute
    let mut fewer = rows_back;
    fewer.remove(3);
    let problems = quadra_store::verify::verify_with(&url, m, r, at(2026, 12, 1), Some(fewer))
        .unwrap()
        .0;
    assert!(
        problems
            .iter()
            .any(|p| p.contains("content does not match its hash")),
        "{problems:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The checkpoint through the sequencer (the cloud, a server): a commit of the log like any
/// other, so a database rebuilt from the log has it, and both verify.
#[test]
fn a_checkpoint_goes_through_the_sequencer_and_its_log() {
    use quadra_store::segments::Folder;
    use quadra_store::sequencer::{Options, Rule};
    quadra_store::db::seed_all_salts();
    let dir = tmp("sequenced");
    let (p, url) = setup(&dir);
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    ops::init(&url, m, r, at(2024, 1, 1)).unwrap();
    for (n, d) in (1..=30).zip((0..30).map(|k| at(2024, 2, 1) + chrono::Duration::days(k))) {
        set(
            &url,
            m,
            r,
            d,
            "Note",
            "text",
            V::Text(format!("note {n}").into()),
        );
    }
    let log = dir.join("log");
    let options = Options {
        destinations: vec![Box::new(Folder::new(&log).unwrap())],
        rule: Rule::All,
        ..Default::default()
    };
    let s = quadra_store::Sequencer::start(&url, options).unwrap();
    let c = quadra_store::archive::archive_with(
        &url,
        m,
        r,
        at(2025, 1, 1),
        at(2026, 12, 1),
        Some(&s),
        &mut |_, _| Ok(()),
    )
    .unwrap()
    .expect("values to archive");
    assert!(c.rows >= 29, "{} rows", c.rows);
    s.stop();
    assert_eq!(
        ops::verify(&url, m, r, at(2026, 12, 1)).unwrap().0,
        Vec::<String>::new()
    );
    let copy = dir.join("copy.db");
    Folder::new(&log).unwrap().restore(&copy).unwrap();
    let curl = format!("sqlite:{}", copy.display());
    assert_eq!(history_rows(&curl), history_rows(&url));
    assert_eq!(
        ops::verify(&curl, m, r, at(2026, 12, 1)).unwrap().0,
        Vec::<String>::new()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
