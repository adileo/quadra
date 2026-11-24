//! Interface files (`.qdi`) are checked with the model: a field renamed in a `.qd` file is an
//! error at the line of every `.qdi` that uses it; names and expressions become references.

use quadra_sema::{Project, Severity, Target};
use std::path::{Path, PathBuf};

fn stress_sources() -> (PathBuf, Vec<(PathBuf, String)>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/stress");
    let root = std::fs::canonicalize(root).unwrap();
    let project = Project::load(&root).unwrap();
    let sources = project.files().filter(|f| f.kind != quadra_sema::FileKind::Std).map(|f| (f.path.clone(), f.text.clone())).collect();
    (root, sources)
}

fn errors(root: &Path, sources: Vec<(PathBuf, String)>) -> Vec<(String, usize, usize, String)> {
    let project = Project::from_sources(root, sources);
    let checked = quadra_analysis::check(&project);
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| {
            let f = project.file(d.file);
            let at = usize::from(d.range.start());
            let line = f.text[..at].matches('\n').count() + 1;
            let col = at - f.text[..at].rfind('\n').map_or(0, |k| k + 1) + 1;
            (f.path.strip_prefix(root).unwrap().display().to_string(), line, col, d.message.clone())
        })
        .collect()
}

#[test]
fn the_examples_interfaces_check() {
    let (root, sources) = stress_sources();
    assert!(sources.iter().any(|(p, _)| p.ends_with("interfaces/sales/cockpit.qdi")));
    let e = errors(&root, sources);
    assert!(e.is_empty(), "{e:#?}");
}

#[test]
fn a_renamed_field_is_an_error_at_the_line_of_the_interface() {
    let (root, mut sources) = stress_sources();
    // `Order.total` becomes `amount_total` in the code (every use of the code follows)
    for (p, t) in sources.iter_mut() {
        if p.extension().is_some_and(|e| e == "qd") {
            *t = t.replace("o.total", "o.amount_total").replace("    total: Money<EUR> = lines", "    amount_total: Money<EUR> = lines");
        }
    }
    let e = errors(&root, sources);
    let cockpit: Vec<_> = e.iter().filter(|x| x.0 == "interfaces/sales/cockpit.qdi").collect();
    // the KPI's source, in an expression
    assert!(cockpit.iter().any(|x| x.1 == 25 && x.2 == 86 && x.3.contains("no field `total`")), "{e:#?}");
    // the board's fields, a list of names
    assert!(cockpit.iter().any(|x| x.1 == 80 && x.3.contains("no field `total`") && x.3.contains("did you mean `amount_total`")), "{e:#?}");
    // and the saved views of another file
    assert!(e.iter().any(|x| x.0 == "interfaces/sales/orders.qdi" && x.3.contains("total")), "{e:#?}");
    // nothing about the code itself but the test that still says `o.total`
    assert!(e.iter().all(|x| x.0.starts_with("interfaces/") || x.0.starts_with("tests/")), "{e:#?}");
}

#[test]
fn a_removed_action_is_an_error_at_the_button() {
    let (root, mut sources) = stress_sources();
    for (p, t) in sources.iter_mut() {
        if p.ends_with("sales.qd") {
            *t = t.replace("        Draft | Confirmed -> Cancelled    by cancel\n", "");
        }
        if p.ends_with("security.qd") {
            *t = t.replace("run Order.confirm, Order.cancel where", "run Order.confirm where");
        }
    }
    let e = errors(&root, sources);
    assert!(e.iter().any(|x| x.0 == "interfaces/sales/cockpit.qdi" && x.3.contains("no action `cancel`")), "{e:#?}");
}

#[test]
fn interfaces_are_references_of_the_code() {
    let (root, sources) = stress_sources();
    let project = Project::from_sources(&root, sources);
    let checked = quadra_analysis::check(&project);
    let i = checked.interfaces.as_ref().unwrap();
    let cockpit = i.project.files().find(|f| f.path.ends_with("interfaces/sales/cockpit.qdi")).unwrap();
    let order = i.analysis.find_anywhere("Order").unwrap();
    let (owner, total) = i.analysis.lookup_member(order, "total").unwrap();
    let in_cockpit: Vec<_> = i.analysis.references.iter().filter(|r| r.file == cockpit.id && r.target == Target::Member(owner, total)).collect();
    // in expressions and in lists of fields
    assert!(in_cockpit.len() >= 8, "{}", in_cockpit.len());
    for r in &in_cockpit {
        assert_eq!(&cockpit.text[r.range], "total");
    }
    let confirm = i.analysis.lookup_member(order, "confirm").unwrap();
    assert!(i.analysis.references.iter().any(|r| r.file == cockpit.id && r.target == Target::Member(confirm.0, confirm.1) && &cockpit.text[r.range] == "confirm"));
    assert!(i.analysis.references.iter().any(|r| r.file == cockpit.id && r.target == Target::Decl(order)));
}
