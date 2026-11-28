//! How much history an entity keeps: `history none` keeps only the values in force (one row of
//! the history per value, outside the chain of commits), `history last 2` the last two versions
//! of each value (the older leave the history at the sweep, their digests kept), and the
//! project's `[data] history` is the default of its entities. The records read back the same,
//! `verify` passes, and a record of `history none` erased leaves nothing.

use quadra_runtime::{Interp, V};
use quadra_sema::Project;
use quadra_store::{materialized_formulas, ops, Conn, Database, DbSource, Sink};
use std::path::{Path, PathBuf};

const MODEL: &str = r#"
entity Page:
    history none
    title: Text
    body: Text?
    author: Text? personal

entity Note:
    history last 2
    text: Text

entity Audit:
    what: Text

seed:
    Page(title="Home", body="v0", author="ada")
    Note(text="n0")
    Audit(what="a0")
"#;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quadra-history-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn at(d: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 3, d)
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
    d: u32,
    ent: &str,
    f: &str,
    v: &str,
) {
    let mut i = engine(url, m, r, at(d));
    let rec = V::Ref {
        ent: ent.into(),
        id: i.ids(ent, None)[0],
    };
    i.transaction(|i| i.set(&rec, f, V::Text(v.into()), None))
        .unwrap();
}

fn read(url: &str, m: &quadra_ir::Model, r: &quadra_types::Report, ent: &str, f: &str) -> String {
    let mut i = engine(url, m, r, at(28));
    let id = i.ids(ent, None)[0];
    i.store
        .read(id, f, None)
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// (value, whether it is in the chain) of the history rows of a field of an entity, in order.
fn rows(url: &str, ent: &str, f: &str) -> Vec<(Option<String>, bool)> {
    Conn::open(url)
        .unwrap()
        .query(&format!("SELECT h.\"value\", h.\"salt\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE r.\"entity\" = '{ent}' AND h.\"field\" = '{f}' ORDER BY h.\"seq\""), &[])
        .unwrap()
        .into_iter()
        .map(|r| (r[0].clone(), r[1].as_deref() != Some("")))
        .collect()
}

fn setup(dir: &Path, toml: &str) -> (Project, String) {
    let pdir = dir.join("project");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(
        pdir.join("quadra.toml"),
        format!("[project]\nname = \"h\"\n{toml}"),
    )
    .unwrap();
    std::fs::write(pdir.join("main.qd"), MODEL).unwrap();
    let p = Project::load(&pdir).unwrap();
    let url = format!("sqlite:{}", dir.join("data.db").display());
    (p, url)
}

#[test]
fn history_none_and_last_keep_what_they_say() {
    quadra_store::db::seed_all_salts();
    let dir = tmp("keep");
    let (p, url) = setup(&dir, "");
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    assert_eq!(m.entity("Page").unwrap().keep, quadra_ir::Keep::None);
    ops::init(&url, m, r, at(1)).unwrap();
    for d in 2..=6 {
        set(&url, m, r, d, "Page", "body", &format!("v{d}"));
        set(&url, m, r, d, "Note", "text", &format!("n{d}"));
        set(&url, m, r, d, "Audit", "what", &format!("a{d}"));
    }
    // none: one row, the value in force, outside the chain
    assert_eq!(
        rows(&url, "Page", "body"),
        vec![(Some("\"v6\"".into()), false)]
    );
    assert_eq!(read(&url, m, r, "Page", "body"), "v6");
    assert_eq!(read(&url, m, r, "Page", "title"), "Home");
    // full: every version
    assert_eq!(rows(&url, "Audit", "what").len(), 6);
    // last 2: every version until the sweep, then the last two (the older keep their digest)
    assert_eq!(rows(&url, "Note", "text").len(), 6);
    let (problems, _) = ops::verify(&url, m, r, at(7)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    ops::sweep(&url, m, r, at(8)).unwrap();
    let notes = rows(&url, "Note", "text");
    assert_eq!(notes.len(), 6);
    let kept: Vec<_> = notes.iter().filter_map(|(v, _)| v.clone()).collect();
    assert_eq!(kept, vec!["\"n5\"".to_string(), "\"n6\"".to_string()]);
    assert_eq!(read(&url, m, r, "Note", "text"), "n6");
    let (problems, _) = ops::verify(&url, m, r, at(9)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let (problems, _) = ops::verify_in_memory(&url, m, r, at(9)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    // an erasure of a value of `history none` leaves nothing of it
    let page = {
        let mut i = engine(&url, m, r, at(10));
        i.ids("Page", None)[0]
    };
    ops::erase(&url, m, r, "Page", page, "asked", at(10), "test").unwrap();
    let author = rows(&url, "Page", "author");
    assert!(
        author
            .iter()
            .all(|(v, _)| v.as_deref().is_none_or(|v| !v.contains("ada"))),
        "{author:?}"
    );
    let (problems, _) = ops::verify(&url, m, r, at(11)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_project_says_what_its_entities_keep() {
    let dir = tmp("default");
    let (p, _) = setup(&dir, "\n[data]\nhistory = \"last 5\"\n");
    let checked = quadra_analysis::check(&p);
    let m = checked.model.as_ref().unwrap();
    assert_eq!(m.entity("Audit").unwrap().keep, quadra_ir::Keep::Last(5));
    assert_eq!(
        m.entity("Page").unwrap().keep,
        quadra_ir::Keep::None,
        "its own line"
    );
    assert_eq!(
        m.entity("User").unwrap().keep,
        quadra_ir::Keep::Full,
        "the standard library's"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
