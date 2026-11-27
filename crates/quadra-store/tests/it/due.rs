//! The next sweep a project needs (`ops::next_due`): the nearest threshold of its rules that read
//! the clock, after its last sweep.

use chrono::{NaiveDate, NaiveDateTime};
use quadra_sema::Project;
use quadra_store::ops::{self, Due};
use quadra_store::Database;
use std::path::Path;

const SRC: &str = "entity Reminder:\n    at: DateTime\n\nentity Fired:\n    reminder: Reminder\n\non Reminder becomes at <= now() as r:\n    Fired(reminder=r)\n\nseed:\n    Reminder(at=2026-12-01 10:00)\n    Reminder(at=2026-12-03 10:00)\n";

fn t(d: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 12, d)
        .unwrap()
        .and_hms_opt(h, m, s)
        .unwrap()
}

#[test]
fn the_next_sweep_is_at_the_nearest_reminder_after_the_last_sweep() {
    let root = Path::new("/due-test");
    let p = Project::from_sources(root, vec![(root.join("main.qd"), SRC.to_string())]);
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let file = std::env::temp_dir().join(format!("quadra-due-{}.db", std::process::id()));
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", file.display()));
    }
    let url = format!("sqlite:{}", file.display());
    ops::init(&url, m, r, t(1, 0, 0, 0) - chrono::Duration::days(1)).unwrap();
    // the instants as the database holds them (the project's zone applied)
    let mut c = quadra_store::Conn::open(&url).unwrap();
    let ats: Vec<NaiveDateTime> = c
        .query("SELECT \"at\" FROM \"reminder\" ORDER BY \"at\"", &[])
        .unwrap()
        .iter()
        .map(|r| {
            NaiveDateTime::parse_from_str(r[0].as_deref().unwrap(), "%Y-%m-%d %H:%M:%S").unwrap()
        })
        .collect();
    assert_eq!(ats.len(), 2);
    let second = chrono::Duration::seconds(1);
    // never swept: the first one
    assert_eq!(
        ops::next_due(&url, m, r, t(1, 0, 0, 0) - chrono::Duration::days(1)).unwrap(),
        Due::At(Some(ats[0] + second))
    );
    // swept after the first: the second
    Database::open(&url, m, r)
        .unwrap()
        .set_meta(
            "last_sweep",
            &(ats[0] + chrono::Duration::hours(1))
                .format("%Y-%m-%dT%H:%M:%S%.f")
                .to_string(),
        )
        .unwrap();
    assert_eq!(
        ops::next_due(&url, m, r, t(1, 0, 0, 0)).unwrap(),
        Due::At(Some(ats[1] + second))
    );
    // the sweep at that instant fires it, and none is ahead any more
    let fired = |c: &mut quadra_store::Conn| {
        c.query("SELECT CAST(COUNT(*) AS TEXT) FROM \"fired\"", &[])
            .unwrap()[0][0]
            .clone()
            .unwrap()
    };
    ops::sweep(&url, m, r, ats[1] + second).unwrap();
    assert_eq!(
        fired(&mut c),
        "1",
        "the second reminder fired, the first had before the last sweep"
    );
    assert_eq!(
        ops::next_due(&url, m, r, t(1, 0, 0, 0)).unwrap(),
        Due::At(None)
    );
}

/// A rule on `today()` against a stored date: the next sweep is at a midnight of the project's
/// zone (the day of a task or the day after), not at every periodic interval.
#[test]
fn a_rule_on_today_sweeps_at_the_midnight_of_the_project_zone() {
    const TASKS: &str = "entity Task:\n    due: Date\n\nentity Late:\n    task: Task\n\non Task becomes due < today() as t:\n    Late(task=t)\n\nseed:\n    Task(due=2026-12-03)\n";
    let root = Path::new("/due-dates");
    let mut p = Project::from_sources(root, vec![(root.join("main.qd"), TASKS.to_string())]);
    p.timezone = "America/New_York".into();
    let checked = quadra_analysis::check(&p);
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let file = std::env::temp_dir().join(format!("quadra-due-dates-{}.db", std::process::id()));
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", file.display()));
    }
    let url = format!("sqlite:{}", file.display());
    ops::init(&url, m, r, t(1, 0, 0, 0)).unwrap();
    let second = chrono::Duration::seconds(1);
    // midnight of 3 December in New York is 05:00 UTC; then the day after
    let set = |at: NaiveDateTime| {
        Database::open(&url, m, r)
            .unwrap()
            .set_meta("last_sweep", &at.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
            .unwrap()
    };
    set(t(1, 0, 0, 0));
    assert_eq!(
        ops::next_due(&url, m, r, t(1, 0, 0, 0)).unwrap(),
        Due::At(Some(t(3, 5, 0, 0) + second))
    );
    set(t(3, 6, 0, 0));
    assert_eq!(
        ops::next_due(&url, m, r, t(3, 6, 0, 0)).unwrap(),
        Due::At(Some(t(4, 5, 0, 0) + second))
    );
    // the sweep then: the task is late
    ops::sweep(&url, m, r, t(4, 5, 0, 1)).unwrap();
    let mut c = quadra_store::Conn::open(&url).unwrap();
    assert_eq!(
        c.query("SELECT CAST(COUNT(*) AS TEXT) FROM \"late\"", &[])
            .unwrap()[0][0]
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        ops::next_due(&url, m, r, t(4, 6, 0, 0)).unwrap(),
        Due::At(None)
    );
}
