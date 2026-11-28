//! `deletable`: a record deleted for good leaves its table (its history stays when the entity
//! keeps it whole); with a trash it goes there first, out of every list and of its unique
//! values, and `restore` gives it back with its id, its lines and the references emptied when
//! it went; the sweep deletes it for good when its time is over, and then a `history none`
//! entity keeps nothing of it. A required reference or a retention holds a record.

use quadra_runtime::{Interp, V};
use quadra_sema::Project;
use quadra_store::{materialized_formulas, ops, Conn, Database, DbSource, Sink};
use std::path::PathBuf;

const MODEL: &str = r#"
entity Page:
    deletable, trash 30 days
    history none
    slug: Text unique
    title: Text
    parent: Page?

entity Block:
    page: Page parent
    text: Text

entity Comment:
    deletable
    page: Page
    text: Text

entity Session:
    deletable
    token: Text

entity Invoice:
    deletable
    retain 10 years after created_at
    number: Text

seed:
    let home = Page(slug="home", title="Home")
    Block(page=home, text="hello")
    Page(slug="about", title="About", parent=home)
    Comment(page=home, text="nice")
    Session(token="abc")
    Invoice(number="1")
"#;

fn at(d: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
        + chrono::Duration::days(d as i64)
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

fn find(i: &mut Interp, ent: &str, field: &str, value: &str) -> Option<u64> {
    i.ids(ent, None).into_iter().find(|id| {
        i.store
            .read(*id, field, None)
            .is_some_and(|v| v.to_string() == value)
    })
}

fn rec(ent: &str, id: u64) -> V {
    V::Ref {
        ent: ent.into(),
        id,
    }
}

fn count(url: &str, sql: &str) -> usize {
    Conn::open(url).unwrap().query(sql, &[]).unwrap().len()
}

#[test]
fn delete_trash_restore_and_for_good() {
    quadra_store::db::seed_all_salts();
    let dir: PathBuf = std::env::temp_dir().join(format!("quadra-deletion-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(dir.join("p/quadra.toml"), "[project]\nname = \"d\"\n").unwrap();
    std::fs::write(dir.join("p/main.qd"), MODEL).unwrap();
    let p = Project::load(&dir.join("p")).unwrap();
    let checked = quadra_analysis::check(&p);
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let url = format!("sqlite:{}", dir.join("data.db").display());
    ops::init(&url, m, r, at(0)).unwrap();

    // a session: for good; its history stays (the entity keeps it whole)
    let mut i = engine(&url, m, r, at(1));
    let s = find(&mut i, "Session", "token", "abc").unwrap();
    i.transaction(|i| i.delete(&rec("Session", s))).unwrap();
    drop(i);
    assert_eq!(count(&url, "SELECT 1 FROM \"session\""), 0);
    assert_eq!(count(&url, &format!("SELECT 1 FROM \"_quadra_history\" WHERE \"record_id\" = {s} AND \"value\" IS NOT NULL")), 1);

    // an invoice is kept ten years
    let mut i = engine(&url, m, r, at(1));
    let inv = find(&mut i, "Invoice", "number", "1").unwrap();
    let e = i
        .transaction(|i| i.delete(&rec("Invoice", inv)))
        .unwrap_err();
    assert!(
        format!("{e:?}").contains("cannot be deleted before 2036-03-01"),
        "{e:?}"
    );
    // a comment holds its page
    let home = find(&mut i, "Page", "slug", "home").unwrap();
    let e = i.transaction(|i| i.delete(&rec("Page", home))).unwrap_err();
    assert!(
        format!("{e:?}").contains("`Comment.page` is required"),
        "{e:?}"
    );
    let c = find(&mut i, "Comment", "text", "nice").unwrap();
    i.transaction(|i| i.delete(&rec("Comment", c))).unwrap();
    // the page to the trash, with its block; the page under it loses its parent
    i.transaction(|i| i.delete(&rec("Page", home))).unwrap();
    drop(i);
    assert_eq!(count(&url, "SELECT 1 FROM \"page\""), 1);
    assert_eq!(count(&url, "SELECT 1 FROM \"block\""), 0);
    assert_eq!(count(&url, "SELECT 1 FROM \"_quadra_trash\""), 1);
    let mut i = engine(&url, m, r, at(2));
    assert_eq!(find(&mut i, "Page", "slug", "home"), None);
    let about = find(&mut i, "Page", "slug", "about").unwrap();
    assert_eq!(
        i.store.read(about, "parent", None).unwrap_or(V::None),
        V::None
    );
    // its slug is free: a new home, then that one deleted too
    i.transaction(|i| {
        i.create(
            "Page",
            vec![
                ("slug".into(), V::Text("home".into())),
                ("title".into(), V::Text("New".into())),
            ],
        )
        .map(|_| ())
    })
    .unwrap();
    let new = find(&mut i, "Page", "slug", "home").unwrap();
    assert_ne!(new, home);
    i.transaction(|i| i.delete(&rec("Page", new))).unwrap();
    drop(i);
    // restored: the same id, its block, its child's parent
    let mut i = engine(&url, m, r, at(3));
    i.transaction(|i| i.restore(&rec("Page", home))).unwrap();
    drop(i);
    let mut i = engine(&url, m, r, at(3));
    assert_eq!(find(&mut i, "Page", "slug", "home"), Some(home));
    assert_eq!(i.store.read(about, "parent", None), Some(rec("Page", home)));
    assert_eq!(i.ids("Block", None).len(), 1);
    drop(i);
    assert_eq!(
        count(&url, "SELECT 1 FROM \"_quadra_trash\""),
        1,
        "the new home is still there"
    );
    let (problems, _) = ops::verify(&url, m, r, at(4)).unwrap();
    assert_eq!(problems, Vec::<String>::new());

    // thirty days later the sweep deletes the new home for good: of a `history none` page
    // nothing is left
    ops::sweep(&url, m, r, at(40)).unwrap();
    assert_eq!(count(&url, "SELECT 1 FROM \"_quadra_trash\""), 0);
    assert_eq!(count(&url, &format!("SELECT 1 FROM \"_quadra_history\" WHERE \"record_id\" = {new} AND \"value\" IS NOT NULL")), 0);
    let (problems, _) = ops::verify(&url, m, r, at(41)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
