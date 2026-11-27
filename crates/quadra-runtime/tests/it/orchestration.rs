//! Workflow orchestration on the interpreter: an order process with a join, a 24-hour wait,
//! timeouts and a cancellation, written with `becomes` reactions and driven by `clock`. The
//! project is `tests/orchestration` (its model in `main.qd`, one file of tests per part).

use quadra_sema::Project;
use std::path::Path;

/// The model with one file of its tests.
fn run(tests: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/orchestration");
    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
    let files = ["main.qd", &format!("tests/{tests}.qd")]
        .iter()
        .map(|p| (root.join(p), read(p)))
        .collect();
    let project = Project::from_sources(&root, files);
    let results = quadra_runtime::run_tests(&project, None).unwrap_or_else(|d| {
        let msgs: Vec<String> = d.iter().map(|d| d.message.clone()).collect();
        panic!("does not check:\n{}", msgs.join("\n"))
    });
    let failed: Vec<String> = results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.name,
                r.location,
                r.message.clone().unwrap_or_default()
            )
        })
        .collect();
    assert!(!results.is_empty(), "no tests found");
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// Payment, approval and picking, in any order or all in one transaction: the order ships
/// once, when the last of them comes.
#[test]
fn a_join_fires_on_its_last_condition_whichever_it_is() {
    run("join");
}

/// `becomes state is Shipped and now() >= shipped_at + 24 hours`: not at 23:59, at 24:00,
/// and once.
#[test]
fn a_24_hour_wait_fires_when_the_clock_passes_it_and_not_before() {
    run("wait");
}

/// Not approved within 4 hours: escalated once; not paid within 48 hours: cancelled, and a
/// late payment ships nothing.
#[test]
fn timeouts_escalate_and_cancel() {
    run("timeouts");
}

/// A cancelled order is neither escalated nor completed when its times come.
#[test]
fn a_cancellation_stops_what_was_waiting() {
    run("cancel");
}

/// A `clock` past a threshold fires the rule there, as the server's sweep would, not at the
/// next statement.
#[test]
fn the_clock_alone_fires_what_was_due() {
    run("clock");
}
