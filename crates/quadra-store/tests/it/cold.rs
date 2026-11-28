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
