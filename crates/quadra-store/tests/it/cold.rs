//! When a record is closed and may go cold (`archive 1 year after date`): its term is over,
//! it was not written for as long, and no transition can take it out of its state now (a
//! guard counts: a paid order cannot be cancelled). `quadra db cold` says how many and why not.

use quadra_runtime::V;
use quadra_sema::Project;
use quadra_store::ops;

const MODEL: &str = r#"
entity Order:
    archive 1 year after date
    date: Date
    paid: Decimal default 0
    note: Text?
    state: enum(Open, Done, Cancelled) default Open
    flow state:
        Open -> Done by finish
        Done -> Cancelled by cancel unless paid > 0

    action finish:
        note = "done"

    action cancel:
        note = "cancelled"
"#;

fn at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(9, 0, 0)
        .unwrap()
}

#[test]
fn a_record_is_closed_when_nothing_can_change_it() {
    let dir = std::env::temp_dir().join(format!("quadra-cold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(dir.join("p/quadra.toml"), "[project]\nname = \"c\"\n").unwrap();
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
    ops::init(&url, m, r, at(2026, 1, 1)).unwrap();
    let mut i = ops::engine(&url, m, r, at(2026, 1, 10)).unwrap();
    let date = V::Date(chrono::NaiveDate::from_ymd_opt(2026, 1, 10).unwrap());
    // open; done and paid (closed); done and unpaid (it can still be cancelled)
    let mut ids = Vec::new();
    for (paid, finish) in [(0, false), (10, true), (0, true)] {
        let o = i
            .transaction(|i| {
                i.create(
                    "Order",
                    vec![
                        ("date".into(), date.clone()),
                        ("paid".into(), V::Dec(paid.into())),
                    ],
                )
            })
            .unwrap();
        if finish {
            i.transaction(|i| {
                i.run_action(&o, "Order", "finish", None, Vec::new(), Vec::new())
                    .map(|_| ())
            })
            .unwrap();
        }
        ids.push(o);
    }
    drop(i);
    let line = |t| ops::cold_report(&url, m, r, t).unwrap().remove(0);
    let l = line(at(2026, 6, 1));
    assert_eq!((l.records, l.closed), (3, 0));
    assert_eq!(l.not_closed, vec![("not yet".to_string(), 3)]);
    let l = line(at(2027, 6, 1));
    assert_eq!(l.closed, 1, "{l:?}");
    assert_eq!(l.not_closed, vec![("open".to_string(), 2)], "{l:?}");
    // written again: not closed for another year
    let mut i = ops::engine(&url, m, r, at(2027, 5, 1)).unwrap();
    i.transaction(|i| i.set(&ids[1], "note", V::Text("a call".into()), None))
        .unwrap();
    drop(i);
    let l = line(at(2027, 6, 1));
    assert_eq!(l.closed, 0, "{l:?}");
    assert!(
        l.not_closed
            .iter()
            .any(|(w, n)| w == "written recently" && *n == 1),
        "{l:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

const SHOP: &str = r#"
entity Customer:
    archive never
    name: Text

entity Order:
    archive 1 year after date
    code: Text unique
    customer: Customer
    date: Date
    paid: Decimal default 0
    note: Text?
    lines: [Line]
    total = lines.sum(l => l.amount)
    state: enum(Open, Done) default Open
    flow state:
        Open -> Done by finish

    action finish:
        note = "done"

entity Line:
    order: Order parent
    amount: Decimal

fn paid_some() -> Int:
    return Order.where(o => o.paid >= 1).count()

fn done() -> Int:
    return Order.where(o => o.state is Done).count()

fn amounts() -> Decimal:
    return Line.all().sum(l => l.amount)
"#;

fn count(url: &str, sql: &str) -> usize {
    quadra_store::Conn::open(url)
        .unwrap()
        .query(sql, &[])
        .unwrap()
        .len()
}

/// Closed records moved to a volume: out of the database in use, and every read still finds
/// them (by id, `all()`, a query in SQL, a total, their lines); `unique` sees them; a write
/// brings one back; the chain verifies.
#[test]
fn cold_records_are_read_as_before() {
    let dir = std::env::temp_dir().join(format!("quadra-cold-move-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(dir.join("p/quadra.toml"), "[project]\nname = \"c\"\n").unwrap();
    std::fs::write(dir.join("p/main.qd"), SHOP).unwrap();
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
    ops::init(&url, m, r, at(2026, 1, 1)).unwrap();
    let mut i = ops::engine(&url, m, r, at(2026, 1, 10)).unwrap();
    let date = V::Date(chrono::NaiveDate::from_ymd_opt(2026, 1, 10).unwrap());
    let anna = i
        .transaction(|i| i.create("Customer", vec![("name".into(), V::Text("Anna".into()))]))
        .unwrap();
    for k in 0..6 {
        let o = i
            .transaction(|i| {
                let o = i.create(
                    "Order",
                    vec![
                        ("code".into(), V::Text(format!("O{k}").into())),
                        ("customer".into(), anna.clone()),
                        ("date".into(), date.clone()),
                        ("paid".into(), V::Dec(k.into())),
                    ],
                )?;
                i.create(
                    "Line",
                    vec![
                        ("order".into(), o.clone()),
                        ("amount".into(), V::Dec((10 + k).into())),
                    ],
                )?;
                Ok(o)
            })
            .unwrap();
        if k < 4 {
            i.transaction(|i| {
                i.run_action(&o, "Order", "finish", None, Vec::new(), Vec::new())
                    .map(|_| ())
            })
            .unwrap();
        }
    }
    drop(i);
    let before = |url: &str| {
        let mut i = ops::engine(url, m, r, at(2027, 6, 1)).unwrap();
        let ids = i.ids("Order", None);
        let total: Vec<String> = ids
            .iter()
            .map(|id| {
                i.get(
                    &V::Ref {
                        ent: "Order".into(),
                        id: *id,
                    },
                    "total",
                    None,
                    None,
                )
                .unwrap()
                .to_string()
            })
            .collect();
        (ids.len(), total, i.ids("Line", None).len())
    };
    let seen = before(&url);
    let mut kept = Vec::new();
    let (vol, n) = ops::cold_run(&url, m, r, at(2027, 6, 1), None, &mut |k, b| {
        kept.push((k.to_string(), b.len()));
        Ok(())
    })
    .unwrap()
    .expect("closed records");
    // four orders done, with their lines
    assert_eq!(n, 8, "{vol}");
    assert_eq!(kept.len(), 1);
    assert_eq!(count(&url, "SELECT 1 FROM \"main\".\"order\""), 2);
    assert_eq!(count(&url, "SELECT 1 FROM \"main\".\"line\""), 2);
    assert_eq!(count(&url, "SELECT 1 FROM \"_quadra_cold\""), 8);
    // every read as before
    assert_eq!(before(&url), seen);
    let mut i = ops::engine(&url, m, r, at(2027, 6, 2)).unwrap();
    let q = |i: &mut quadra_runtime::Interp, name: &str| {
        let f = m.function(name).unwrap();
        let quadra_ir::StmtKind::Return(Some(e)) = &f.body.as_ref().unwrap()[0].kind else {
            panic!("{name}")
        };
        i.eval(e, &mut quadra_runtime::Ctx::default())
            .unwrap()
            .to_string()
    };
    assert_eq!(q(&mut i, "paid_some"), "5");
    assert_eq!(q(&mut i, "done"), "4");
    assert_eq!(q(&mut i, "amounts"), "75");
    // a code a cold order has is taken
    let e = i
        .transaction(|i| {
            i.create(
                "Order",
                vec![
                    ("code".into(), V::Text("O1".into())),
                    ("customer".into(), anna.clone()),
                    ("date".into(), date.clone()),
                ],
            )
            .map(|_| ())
        })
        .unwrap_err();
    assert!(
        format!("{e:?}").contains("unique") || format!("{e:?}").contains("already"),
        "{e:?}"
    );
    // written: back in use
    let o1 = i
        .ids("Order", None)
        .into_iter()
        .find(|id| i.store.read(*id, "code", None).map(|v| v.to_string()) == Some("O1".into()))
        .unwrap();
    i.transaction(|i| {
        i.set(
            &V::Ref {
                ent: "Order".into(),
                id: o1,
            },
            "note",
            V::Text("a call".into()),
            None,
        )
    })
    .unwrap();
    drop(i);
    assert_eq!(count(&url, "SELECT 1 FROM \"main\".\"order\""), 3);
    assert_eq!(count(&url, "SELECT 1 FROM \"_quadra_cold\""), 7);
    let mut i = ops::engine(&url, m, r, at(2027, 6, 3)).unwrap();
    assert_eq!(
        i.store.read(o1, "note", None).map(|v| v.to_string()),
        Some("a call".into())
    );
    assert_eq!(i.ids("Order", None).len(), 6);
    drop(i);
    let (problems, _) = ops::verify(&url, m, r, at(2027, 6, 4)).unwrap();
    assert_eq!(problems, Vec::<String>::new());

    // a deploy that changes only the customers: the orders stay cold
    let deploy = |src: &str, t| {
        std::fs::write(dir.join("p/main.qd"), src).unwrap();
        let p = Project::load(&dir.join("p")).unwrap();
        let c = quadra_analysis::check(&p);
        assert!(
            c.model.is_some(),
            "{:?}",
            c.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        ops::deploy(
            &url,
            c.model.as_ref().unwrap(),
            &c.report,
            &dir.join("p"),
            t,
            false,
            "test",
        )
        .unwrap();
        c
    };
    let c2 = deploy(
        &SHOP.replace("    name: Text\n", "    name: Text\n    phone: Text?\n"),
        at(2027, 7, 1),
    );
    assert_eq!(count(&url, "SELECT 1 FROM \"_quadra_cold\""), 7);
    // one that adds a field with a default to the orders: they come back first
    let c3 = deploy(
        &SHOP
            .replace("    name: Text\n", "    name: Text\n    phone: Text?\n")
            .replace(
                "    note: Text?\n",
                "    note: Text?\n    channel: Text default \"shop\"\n",
            ),
        at(2027, 7, 2),
    );
    let _ = c2;
    assert_eq!(
        count(
            &url,
            "SELECT 1 FROM \"_quadra_cold\" WHERE \"entity\" = 'Order'"
        ),
        0
    );
    assert_eq!(count(&url, "SELECT 1 FROM \"main\".\"order\""), 6);
    let (m3, r3) = (c3.model.as_ref().unwrap(), &c3.report);
    let mut i = ops::engine(&url, m3, r3, at(2027, 7, 3)).unwrap();
    let ids = i.ids("Order", None);
    assert_eq!(ids.len(), 6);
    assert!(ids.iter().all(
        |id| i.store.read(*id, "channel", None).map(|v| v.to_string()) == Some("shop".into())
    ));
    drop(i);
    let (problems, _) = ops::verify(&url, m3, r3, at(2027, 7, 4)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Through the sequencer and its log (a server, the cloud): the move, a write that brings a
/// record back, an erasure of a cold record (its value gone from the database in use and the
/// view); a database rebuilt from the log reads the same, with the volume.
#[test]
fn cold_through_the_sequencer() {
    use quadra_store::segments::Folder;
    use quadra_store::sequencer::{Options, Rule};
    quadra_store::db::seed_all_salts();
    let dir = std::env::temp_dir().join(format!("quadra-cold-seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(dir.join("p/quadra.toml"), "[project]\nname = \"c\"\n").unwrap();
    let src = SHOP.replace("entity Customer:\n    archive never\n    name: Text\n", "entity Customer:\n    archive never\n    name: Text\n\nentity Contact:\n    archive 1 year after created_at\n    email: Email? personal\n");
    std::fs::write(dir.join("p/main.qd"), &src).unwrap();
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
    ops::init(&url, m, r, at(2026, 1, 1)).unwrap();
    let log = dir.join("log");
    let s = quadra_store::Sequencer::start(
        &url,
        Options {
            destinations: vec![Box::new(Folder::new(&log).unwrap())],
            rule: Rule::All,
            ..Default::default()
        },
    )
    .unwrap();
    let engine = |t| {
        let mut i = ops::engine(&url, m, r, t).unwrap();
        let db = quadra_store::Database::open(&url, m, r).unwrap();
        i.sink = Some(Box::new(quadra_store::SequencedSink {
            db,
            model: m,
            sequencer: s.clone(),
            tickets: None,
        }));
        i
    };
    let mut i = engine(at(2026, 1, 10));
    let ann = i
        .transaction(|i| {
            i.create(
                "Contact",
                vec![("email".into(), V::Text("ann@example.com".into()))],
            )
        })
        .unwrap();
    let bob = i
        .transaction(|i| {
            i.create(
                "Contact",
                vec![("email".into(), V::Text("bob@example.com".into()))],
            )
        })
        .unwrap();
    drop(i);
    let (_, n) = ops::cold_run(&url, m, r, at(2027, 6, 1), Some(&s), &mut |_, _| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(count(&url, "SELECT 1 FROM \"main\".\"contact\""), 0);
    // erased while cold: back in use, and its value gone
    let mut i = engine(at(2027, 6, 2));
    i.transaction(|i| i.erase(&ann, "asked").map(|_| ()))
        .unwrap();
    drop(i);
    let mut i = engine(at(2027, 6, 3));
    let V::Ref { id: bob_id, .. } = &bob else {
        panic!()
    };
    let V::Ref { id: ann_id, .. } = &ann else {
        panic!()
    };
    assert_eq!(
        i.store.read(*ann_id, "email", None).unwrap_or(V::None),
        V::None
    );
    assert_eq!(
        i.store.read(*bob_id, "email", None).map(|v| v.to_string()),
        Some("bob@example.com".into())
    );
    drop(i);
    assert_eq!(
        count(
            &url,
            "SELECT 1 FROM \"main\".\"_quadra_history\" WHERE \"value\" LIKE '%ann@%'"
        ),
        0
    );
    s.stop();
    let (problems, _) = ops::verify(&url, m, r, at(2027, 6, 4)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    // rebuilt from the log: the same, with the volume beside it
    let copy = dir.join("copy.db");
    Folder::new(&log).unwrap().restore(&copy).unwrap();
    let _ = std::fs::create_dir_all(dir.join("copy.db.cold"));
    for e in std::fs::read_dir(dir.join("data.db.cold"))
        .unwrap()
        .flatten()
    {
        std::fs::copy(e.path(), dir.join("copy.db.cold").join(e.file_name())).unwrap();
    }
    let curl = format!("sqlite:{}", copy.display());
    let (problems, _) = ops::verify(&curl, m, r, at(2027, 6, 4)).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let mut i = ops::engine(&curl, m, r, at(2027, 6, 5)).unwrap();
    assert_eq!(
        i.store.read(*bob_id, "email", None).map(|v| v.to_string()),
        Some("bob@example.com".into())
    );
    assert_eq!(i.ids("Contact", None).len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
