//! The `test` blocks of the example projects run green on the interpreter.

use quadra_sema::Project;
use std::path::Path;

fn run(example: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(example);
    let project = Project::load(&root).unwrap();
    let results = quadra_runtime::run_tests(&project, None)
        .unwrap_or_else(|d| panic!("{example} does not check: {d:#?}"));
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
    assert!(!results.is_empty(), "{example}: no tests found");
    assert!(failed.is_empty(), "{example}:\n{}", failed.join("\n"));
}

#[test]
fn minimal_example_tests_pass() {
    run("minimal");
}

#[test]
fn extended_example_tests_pass() {
    run("extended");
}

/// Records read from and written to CSV, XML and fixed-width files.
#[test]
fn formats_example_tests_pass() {
    run("formats");
}

/// `security.qd`: a Salesperson does not see the product cost; a SalesManager (a Salesperson
/// whose role says `show Product.cost`) does, and still does not see the standard cost.
#[test]
fn extended_example_show_undoes_hide() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let project = Project::load(&root).unwrap();
    let (failure, seen) = quadra_runtime::run_session(
        &project,
        "ordine → consegna → fattura → incasso",
        |_, _, interp, _| {
            let mut out = Vec::new();
            for login in ["giulia", "marco", "anna"] {
                let id = interp.ids("User", None).into_iter().find(|id| {
                    interp
                        .store
                        .read(*id, "login", None)
                        .map_or(false, |v| v.to_string() == login)
                });
                assert!(id.is_some(), "no user {login}");
                interp.user = id;
                out.push((
                    login,
                    interp.hidden("Product", "cost"),
                    interp.hidden("Product", "standard_cost"),
                ));
            }
            out
        },
    )
    .unwrap_or_else(|d| panic!("does not check: {d:#?}"));
    assert_eq!(failure, None);
    // giulia is also a Buyer (`show Product.cost`); anna has no `hide` at all
    assert_eq!(
        seen,
        vec![
            ("giulia", false, true),
            ("marco", false, true),
            ("anna", false, false)
        ]
    );
}
