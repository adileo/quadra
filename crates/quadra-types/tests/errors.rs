//! The type errors the examples promise (`expect_compile_error`) are really reported.

use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

fn errors_with(snippet: &str) -> Vec<String> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let base = Project::load(&root).unwrap();
    let mut sources: Vec<(PathBuf, String)> = base
        .user_files()
        .map(|f| (f.path.clone(), f.text.clone()))
        .collect();
    let is_item = ["extend ", "fn ", "patch ", "entity "]
        .iter()
        .any(|k| snippet.starts_with(k));
    let text = if is_item {
        snippet.to_string()
    } else {
        format!(
            "fn __snippet():\n{}",
            snippet
                .lines()
                .map(|l| format!("    {l}\n"))
                .collect::<String>()
        )
    };
    sources.push((root.join("tests/__snippet.qd"), text));
    let project = Project::from_sources(&root, sources);
    let analysis = project.analyze();
    let mut out: Vec<String> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    if out.is_empty() {
        let model = quadra_ir::lower(&project, &analysis);
        out = quadra_types::check_model(&model)
            .into_iter()
            .map(|d| d.message)
            .collect();
    }
    out
}

fn assert_error(snippet: &str, expected: &str) {
    let errors = errors_with(snippet);
    assert!(
        errors.iter().any(|e| e.contains(expected)),
        "{snippet}\nexpected {expected:?}, got {errors:#?}"
    );
}

#[test]
fn baseline_is_clean() {
    let errors = errors_with("let ok = 10.00 EUR + 5.00 EUR");
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn currencies_and_units() {
    assert_error(
        "let x = 10.00 EUR + 5.00 USD",
        "cannot add Money<EUR> and Money<USD>",
    );
    assert_error("let y = 2 kg + 3 pcs", "cannot add Qty<kg> and Qty<pcs>");
    assert_error(
        "let z = 2 kg * 10.00 EUR/pcs",
        "cannot multiply Qty<kg> by Price<EUR, pcs>",
    );
    assert_error(
        "let all = SalesLine.all().sum(l => l.subtotal)",
        "different currencies",
    );
    assert_error("let w = 3 parsecs", "unknown unit or currency `parsecs`");
    // same category converts: fine
    assert!(errors_with("let g = 2 kg + 300 g").is_empty());
}

#[test]
fn live() {
    assert_error(
        "extend Partner:\n    live display_name\n",
        "Partner is never frozen",
    );
    assert_error(
        "extend SalesOrder:\n    live nothing_like_this\n",
        "`nothing_like_this` is not a field of SalesOrder",
    );
    assert_error(
        "extend SalesOrder:\n    live customer\n",
        "only formulas can be live",
    );
    assert!(errors_with(
        "extend SalesOrder:\n    lines_count = lines.count()\n    live lines_count\n"
    )
    .is_empty());
}

#[test]
fn rounding() {
    assert_error("extend SalesLine:\n    net_price: Price<order.currency, uom> default price * (1 - discount)\n", "must be rounded");
    assert!(errors_with("extend SalesLine:\n    net_price: Price<order.currency, uom> default (price * (1 - discount)).round()\n").is_empty());
}

#[test]
fn optionals() {
    assert_error(
        "fn limit(p: Partner) -> Money?:\n    return p.as_customer.credit_limit\n",
        "is optional",
    );
    assert!(errors_with(
        "fn limit(p: Partner) -> Money?:\n    return p.as_customer?.credit_limit\n"
    )
    .is_empty());
}

#[test]
fn purity_and_exhaustiveness() {
    assert_error(
        "extend StockMove:\n    age = now() - date\n",
        "now() is not allowed in a formula",
    );
    assert_error("extend SalesOrder:\n    label: Text = match state:\n        Draft => \"a\"\n        Sent => \"b\"\n        Confirmed => \"c\"\n        Locked => \"d\"\n", "missing Cancelled");
}

#[test]
fn fields_and_values() {
    assert_error(
        "fn f(o: SalesOrder) -> Bool:\n    return o.totale > 0\n",
        "`SalesOrder` has no field `totale`",
    );
    assert_error(
        "fn f(o: SalesOrder) -> Bool:\n    return o.state is Posted\n",
        "`Posted` is not a value of",
    );
    assert_error(
        "fn f() -> Product:\n    return Product(colour=\"red\")\n",
        "`Product` has no field `colour`",
    );
    assert_error("fn f(o: SalesOrder):\n    o.total = 0\n", "is a formula");
}
