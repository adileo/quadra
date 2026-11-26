//! `examples/extended/tests/trace.expected` is the golden output of the analyses.
//! Regenerate with: UPDATE_GOLDEN=1 cargo test -p quadra-analysis --test it golden

use quadra_sema::{Project, Severity};
use std::path::Path;

#[test]
fn trace_output_matches_golden_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let project = Project::load(&root).unwrap();
    let checked = quadra_analysis::check(&project);
    let errors: Vec<&str> =
        checked.diagnostics.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.as_str()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let model = checked.model.as_ref().unwrap();

    let mut out = String::from(
        "# Golden output of the analyses on this example (crates/quadra-analysis/tests/golden.rs).\n\
         # Regenerate with: UPDATE_GOLDEN=1 cargo test -p quadra-analysis --test it golden\n",
    );
    for target in ["Invoice.due", "SalesOrder.state", "Product.on_hand", "Product.avg_cost"] {
        out.push_str(&format!("\n$ quadra trace {target}\n\n"));
        out.push_str(&quadra_analysis::trace(&project, model, &checked.report, target).unwrap());
    }
    out.push_str("\n$ quadra certify (fingerprints)\n\n");
    for (name, fp, deps) in quadra_analysis::fingerprints(model, &checked.report) {
        out.push_str(&format!("{name:<24}{fp}  {deps} dependencies\n"));
    }

    let golden = root.join("tests/trace.expected");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&golden, &out).unwrap();
    }
    let expected = std::fs::read_to_string(&golden).unwrap();
    assert_eq!(out, expected, "analysis output changed: review it and regenerate with UPDATE_GOLDEN=1");
}

#[test]
fn a_formula_cycle_is_reported() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let base = Project::load(&root).unwrap();
    let mut sources: Vec<_> = base.user_files().map(|f| (f.path.clone(), f.text.clone())).collect();
    sources.push((
        root.join("tests/__patch.qd"),
        "patch StockMove:\n    unit_cost: Price<company.currency, product.uom> = product.cost[company].round()\n".into(),
    ));
    let checked = quadra_analysis::check(&Project::from_sources(&root, sources));
    assert!(checked.diagnostics.iter().any(|d| d.message.starts_with("formula cycle")), "{:#?}", checked.diagnostics);
}
