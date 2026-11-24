use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

fn check(files: &[(&str, &str)]) -> Vec<String> {
    let root = Path::new("/project");
    let sources = files.iter().map(|(p, t)| (root.join(p), t.to_string())).collect();
    let project = Project::from_sources(root, sources);
    let analysis = project.analyze();
    analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| format!("{}: {}", project.file(d.file).module, d.message))
        .collect()
}

fn assert_error(files: &[(&str, &str)], expected: &str) {
    let errors = check(files);
    assert!(errors.iter().any(|e| e.contains(expected)), "expected {expected:?}, got {errors:#?}");
}

#[test]
fn examples_have_no_errors() {
    for example in ["minimal", "extended"] {
        let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(example);
        let project = Project::load(&root).unwrap();
        let analysis = project.analyze();
        let errors: Vec<String> = analysis
            .diagnostics
            .iter()
            .map(|d| format!("{} {:?}: {}", project.file(d.file).module, d.range, d.message))
            .collect();
        assert!(errors.is_empty(), "{example}: {errors:#?}");
    }
}

#[test]
fn imports() {
    assert_error(&[("a.qd", "from nowhere import X\n")], "unknown module `nowhere`");
    assert_error(
        &[("a.qd", "entity A:\n    x: Int\n"), ("b.qd", "from a import B\n")],
        "`B` is not declared in module `a`",
    );
    assert_error(
        &[("a.qd", "from b import B\nentity A:\n    b: B\n"), ("b.qd", "from a import A\nentity B:\n    a: A\n")],
        "import cycle: a → b → a",
    );
    assert_error(
        &[("a.qd", "entity A:\n    x: Int\n"), ("tests/t.qd", "fixture f:\n    let a = 1\n"), ("b.qd", "from tests.t import f\n")],
        "modules cannot import from it",
    );
}

#[test]
fn unknown_names_suggest_imports() {
    let files = [("a.qd", "entity Customer:\n    name: Text\n"), ("b.qd", "entity Order:\n    customer: Customer\n")];
    assert_error(&files, "unknown type `Customer`: it is declared in `a`, add `from a import Customer`");
    assert_error(&[("a.qd", "fn f():\n    return Nope(1)\n")], "unknown name `Nope`");
    // scripts see everything without imports
    let ok = check(&[("a.qd", "entity Customer:\n    name: Text\n"), ("tests/t.qd", "test \"x\":\n    Customer(name=\"a\")\n")]);
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn extend_and_members() {
    let base = ("a.qd", "entity Product:\n    name: Text\n");
    assert_error(
        &[base, ("b.qd", "from a import Product\nextend Product:\n    name: Text\n")],
        "`name` is already declared on `Product` at a.qd:2",
    );
    assert_error(&[("b.qd", "extend Missing:\n    x: Int\n")], "unknown entity, record or trait `Missing`");
    assert_error(&[("a.qd", "entity A:\n    x default 1\n")], "`x` is not declared on `A` or its traits");
    assert_error(&[("a.qd", "entity A:\n    x: Int\n    unique (y)\n")], "`A` has no member `y`");
    // a default override of a trait field is fine
    let ok = check(&[("a.qd", "trait Doc:\n    number: Code?\nentity Order is Doc:\n    number default \"1\"\n")]);
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn flows_and_reactions() {
    let order = "entity Order:\n    state: enum(Draft, Done) default Draft\n    flow state:\n        Draft -> Done by finish when ready\n";
    assert!(check(&[("a.qd", order)]).is_empty(), "{:?}", check(&[("a.qd", order)]));
    assert_error(
        &[("a.qd", "entity Order:\n    state: enum(Draft, Done) default Draft\n    flow state:\n        Draft -> Shipped by ship\n")],
        "`Shipped` is not a state of `state`",
    );
    assert_error(&[("a.qd", order), ("b.qd", "from a import Order\non Order enters Lost as o:\n    fail \"x\"\n")], "`Order` has no state `Lost`");
    assert_error(&[("a.qd", order), ("b.qd", "from a import Order\nbefore Order.state as o:\n    fail \"x\"\n")], "`state` is not an action");
    // `by finish` declares an action usable by `before` and roles
    let ok = check(&[("a.qd", order), ("b.qd", "from a import Order\nbefore Order.finish as o:\n    fail \"x\"\nrole R:\n    run Order.finish\n")]);
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn roles_fixtures_ledgers() {
    assert_error(&[("a.qd", "entity A:\n    x: Int\nrole R:\n    read A.y\n")], "`A` has no member `y`");
    assert_error(&[("tests/t.qd", "test \"x\":\n    use missing\n")], "unknown fixture `missing`");
    assert_error(&[("a.qd", "entity A:\n    x: Int\n    post to Nope:\n        add(x=1)\n")], "unknown ledger `Nope`");
}

#[test]
fn compile_error_blocks_are_not_checked() {
    let ok = check(&[("tests/t.qd", "test \"x\":\n    expect_compile_error \"boom\":\n        let y = Nope(1)\n")]);
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn new_syntax_rules() {
    assert_error(&[("a.qd", "entity A:\n    k: Draft | Done\n")], "anonymous unions are not allowed");
    assert_error(
        &[("a.qd", "entity A:\n    x: Int\n    t = x\n    frozen when x > 0 live x\n")],
        "`live` lists formulas",
    );
    let ok = check(&[(
        "a.qd",
        "trait Line:\n    qty: Int\nentity L is Line:\n    qty: Int\n    total = [1, 2].sum(v => v * qty)\n",
    )]);
    assert!(ok.is_empty(), "{ok:?}");
    // currencies and units from the standard library are values
    let ok = check(&[("a.qd", "fn f() -> Currency:\n    return EUR\n")]);
    assert!(ok.is_empty(), "{ok:?}");
}

/// 24. The field of a flow must have an inline enum type: the message gives today's syntax.
#[test]
fn a_flow_needs_an_inline_enum_field() {
    let msg = "`state` must have an inline enum type to be used in a flow: declare it as `state: enum(Draft, Posted) default Draft`";
    assert_error(&[("a.qd", "entity A:\n    state: Text\n    flow state:\n        Draft -> Posted by post\n")], msg);
    assert_error(&[("a.qd", "enum S: Draft, Posted\n\nentity A:\n    state: S default Draft\n    flow state:\n        Draft -> Posted by post\n")], msg);
    assert!(check(&[("a.qd", "entity A:\n    state: enum(Draft, Posted) default Draft\n    flow state:\n        Draft -> Posted by post\n")]).is_empty());
}
