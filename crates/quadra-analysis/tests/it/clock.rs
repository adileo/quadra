//! When a model next changes with time alone (`quadra_analysis::clock`): thresholds on a stored
//! DateTime are followed exactly, anything else by the periodic sweep.

use quadra_analysis::clock::{schedule, Schedule, Threshold};
use quadra_sema::Project;
use std::path::Path;

fn of(src: &str) -> Schedule {
    let root = Path::new("/clock-test");
    let checked = quadra_analysis::check(&Project::from_sources(
        root,
        vec![(root.join("main.qd"), src.to_string())],
    ));
    let m = checked
        .model
        .as_ref()
        .unwrap_or_else(|| panic!("{:#?}", checked.diagnostics));
    schedule(m)
}

const BASE: &str = "entity Reminder:\n    at: DateTime\n    sent: Bool = false\n\nentity Fired:\n    reminder: Reminder\n\n";

#[test]
fn a_model_without_the_clock_never_needs_a_sweep() {
    assert_eq!(of(BASE), Schedule::Never);
}

#[test]
fn a_comparison_of_now_with_a_stored_datetime_is_a_threshold() {
    let s = of(&format!(
        "{BASE}on Reminder becomes at <= now() and not sent as r:\n    Fired(reminder=r)\n"
    ));
    assert_eq!(
        s,
        Schedule::At(vec![Threshold {
            entity: "Reminder".into(),
            field: "at".into(),
            days: 0,
            secs: 0
        }])
    );
    // either side, with an amount of time
    let s = of(&format!(
        "{BASE}on Reminder becomes now() > at + 2 hours as r:\n    Fired(reminder=r)\n"
    ));
    assert_eq!(
        s,
        Schedule::At(vec![Threshold {
            entity: "Reminder".into(),
            field: "at".into(),
            days: 0,
            secs: 7200
        }])
    );
    let s = of(&format!(
        "{BASE}on Reminder becomes at - 3 days < now() as r:\n    Fired(reminder=r)\n"
    ));
    assert_eq!(
        s,
        Schedule::At(vec![Threshold {
            entity: "Reminder".into(),
            field: "at".into(),
            days: -3,
            secs: 0
        }])
    );
}

#[test]
fn anything_else_on_the_clock_keeps_the_periodic_sweep() {
    for (what, src) in [
        ("today()", format!("entity Task:\n    due: Date\n\nentity Fired:\n    task: Task\n\non Task becomes due < today() as t:\n    Fired(task=t)\n")),
        ("the hour", format!("{BASE}on Reminder becomes now().hour == 9 as r:\n    Fired(reminder=r)\n")),
        ("through a function", format!("{BASE}fn passed(x: DateTime) -> Bool:\n    return x <= now()\n\non Reminder becomes passed(at) as r:\n    Fired(reminder=r)\n")),
        ("beside a threshold, through a function", format!("{BASE}fn late() -> Bool:\n    return now().hour > 18\n\non Reminder becomes at <= now() and late() as r:\n    Fired(reminder=r)\n")),
    ] {
        assert!(matches!(of(&src), Schedule::Periodic(_)), "{what}: {:?}", of(&src));
    }
}
