//! Sources nested deeper than the parser allows are an error, never a stack overflow: every pass
//! after the parser walks the tree recursively, and a project is checked inside the process that
//! serves every other project too.
//!
//! The tests run on threads with 2 MB of stack (the default of a spawned thread), well below the
//! engine's, so the nesting allowed must fit there with room to spare.

use quadra_sema::{Project, Severity};
use std::path::Path;

fn check(source: String) -> Vec<String> {
    let root = Path::new("/depth");
    let project = Project::from_sources(root, vec![(root.join("main.qd"), source)]);
    let checked = quadra_analysis::check(&project);
    checked
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message)
        .collect()
}

fn function(body: &str) -> String {
    format!("fn deep() -> Int:\n    {body}\n")
}

fn too_deep(errors: &[String]) -> bool {
    errors.iter().any(|e| e.contains("nested too deeply"))
}

#[test]
fn parentheses_past_the_limit_are_an_error() {
    let n = 100_000;
    let errors = check(function(&format!("{}1{}", "(".repeat(n), ")".repeat(n))));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn parentheses_within_the_limit_are_checked() {
    let n = 60;
    let errors = check(function(&format!("{}1{}", "(".repeat(n), ")".repeat(n))));
    assert!(!too_deep(&errors), "{errors:?}");
}

#[test]
fn a_long_chain_of_operators_is_an_error() {
    let errors = check(function(&vec!["1"; 100_000].join(" + ")));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn a_chain_of_operators_within_the_limit_is_checked() {
    let errors = check(function(&vec!["1"; 250].join(" + ")));
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_long_chain_of_calls_is_an_error() {
    let errors = check(function(&format!("[1]{}", ".first()".repeat(100_000))));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn nested_lists_are_an_error() {
    let n = 100_000;
    let errors = check(function(&format!("{}1{}", "[".repeat(n), "]".repeat(n))));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn nested_types_are_an_error() {
    let n = 100_000;
    let errors = check(format!(
        "fn deep(x: {}Int{}) -> Int:\n    1\n",
        "[".repeat(n),
        "]".repeat(n)
    ));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn a_long_chain_of_optionals_is_an_error() {
    // spaced, so that the lexer reads single `?` and not `??`
    let errors = check(format!(
        "fn deep(x: Int{}) -> Int:\n    1\n",
        " ?".repeat(100_000)
    ));
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn nested_blocks_are_an_error() {
    let mut source = String::from("fn deep() -> Int:\n");
    for level in 1..=300 {
        source.push_str(&format!("{}if true:\n", "    ".repeat(level)));
    }
    source.push_str(&format!("{}return 1\n", "    ".repeat(301)));
    source.push_str("    return 0\n");
    let errors = check(source);
    assert!(too_deep(&errors), "{errors:?}");
}

#[test]
fn the_line_after_a_too_deep_one_is_parsed_again() {
    let n = 100_000;
    let source = format!(
        "fn deep() -> Int:\n    let x = {}1{}\n    return y\n",
        "(".repeat(n),
        ")".repeat(n)
    );
    let errors = check(source);
    assert!(too_deep(&errors), "{errors:?}");
    // one report for the deep line, not one per level
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.contains("nested too deeply"))
            .count(),
        1,
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains('y')),
        "the next line is still checked: {errors:?}"
    );
}

#[test]
fn the_deepest_tree_allowed_is_checked() {
    // as deep as the parser allows in both ways at once: parentheses as deep as they may go,
    // inside a chain of operators as long as it may grow
    // (the test fails by aborting the process if a later pass overflows its stack)
    let inner = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    for chain in [100, 190, 250] {
        check(function(&format!(
            "{inner} + {}",
            vec!["1"; chain].join(" + ")
        )));
        check(function(&format!("[{inner}]{}", ".first()".repeat(chain))));
    }
}
