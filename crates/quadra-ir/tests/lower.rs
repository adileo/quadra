use quadra_sema::Project;
use std::path::Path;

fn model(example: &str) -> quadra_ir::Model {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(example);
    let project = Project::load(&root).unwrap();
    let analysis = project.analyze();
    quadra_ir::lower(&project, &analysis)
}

#[test]
fn extended_example_lowers() {
    let m = model("extended");
    let product = m.entity("Product").expect("Product");
    let names: Vec<&str> = product.fields.iter().map(|f| f.name.as_str()).collect();
    // fields added by `extend` in accounting, inventory and manufacturing are merged
    for f in ["code", "sale_tax", "on_hand", "cost", "bom"] {
        assert!(names.contains(&f), "Product.{f} missing: {names:?}");
    }
    let stock = m.entity("Stock").unwrap();
    assert!(stock.is_ledger);
    let so = m.entity("SalesOrder").unwrap();
    assert_eq!(so.traits, vec!["Document"]);
    let total = so.field("untaxed").unwrap();
    assert_eq!(
        quadra_ir::print::expr(total.formula.as_ref().unwrap()),
        "lines.sum(l => l.subtotal)"
    );
    assert!(
        m.entity("Company").unwrap().field("vat_number").is_some(),
        "std Company extended"
    );
    assert!(m.tests.len() >= 10);
    assert!(m
        .fixtures
        .iter()
        .any(|f| f.name == "bom_example" && f.extends.as_deref() == Some("italy")));
    assert!(m.reactions.len() >= 5);
    let f = m.entity("Invoice").unwrap().frozen.as_ref().unwrap();
    assert_eq!(f.live, vec!["paid", "due", "payment_state"]);
}

#[test]
fn fstrings_are_parsed() {
    let m = model("extended");
    let cat = m.entity("Category").unwrap();
    let path = quadra_ir::print::expr(cat.field("path").unwrap().formula.as_ref().unwrap());
    assert_eq!(
        path,
        "f\"{parent.path}/{name}\" if (parent != none) else name"
    );
}
