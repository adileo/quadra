//! Regression tests for engine bugs (the known issues): each one is a small project whose
//! `test` blocks must all pass on the interpreter.

use quadra_sema::Project;
use std::path::Path;

fn run(src: &str) {
    run_files(&[("main.qd", src)]);
}

fn project(files: &[(&str, &str)]) -> Project {
    let root = Path::new("/regression");
    Project::from_sources(
        root,
        files
            .iter()
            .map(|(name, src)| (root.join(name), src.to_string()))
            .collect(),
    )
}

/// The compile errors of a project (none: it checks).
fn errors(files: &[(&str, &str)]) -> Vec<String> {
    let checked = quadra_analysis::check(&project(files));
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == quadra_sema::Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

fn assert_error(files: &[(&str, &str)], expected: &str) {
    let errors = errors(files);
    assert!(
        errors.iter().any(|e| e.contains(expected)),
        "expected an error containing {expected:?}, got {errors:#?}"
    );
}

fn run_files(files: &[(&str, &str)]) {
    let project = project(files);
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

#[test]
fn first_and_last_apply_the_predicate() {
    run(r#"
entity Warehouse:
    code: Code unique
    main: Bool default false

test "first/last with a predicate":
    let xs = [1, 2, 3]
    expect xs.first(x => x > 1) == 2
    expect xs.last(x => x < 3) == 2
    expect xs.first() == 1
    expect xs.last() == 3
    expect xs.first(x => x > 5) is none
    Warehouse(code="A")
    Warehouse(code="B", main=true)
    expect Warehouse.first(w => w.main)?.code == "B"
"#);
}

#[test]
fn is_none_tests_for_absence() {
    run(r#"
entity Customer:
    name: Text
    email: Email?

test "is none":
    let x: Int? = none
    expect x is none
    let y: Int? = 3
    expect y is not none
    let c = Customer(name="Anna")
    expect c.email is none
    c.email = "anna@example.com"
    expect c.email is not none
"#);
}

#[test]
fn local_variables_can_be_reassigned() {
    run(r#"
fn total(xs: [Int]) -> Int:
    let t = 0
    for x in xs:
        t = t + x
    return t

fn bump() -> Int:
    let t = 1
    t += 5
    t -= 2
    return t

fn pick(b: Bool) -> Int:
    let t = 1
    if b:
        t = 2
    else:
        t += 10
    return t

test "locals":
    expect total([1, 2, 3]) == 6
    expect bump() == 4
    expect pick(true) == 2
    expect pick(false) == 11
"#);
}

#[test]
fn trait_checks_and_uniques_apply_to_implementors() {
    run(r#"
trait Doc:
    n: Code
    tag: Text? unique
    unique (n)
    check n.len() > 1, "n is too short"

trait Numbered is Doc:
    seq: Int default 1
    check seq > 0, "seq must be positive"

entity Invoice is Doc:
    amount: Int default 0

entity Note is Numbered:
    body: Text default ""

test "trait rules":
    Invoice(n="AB")
    expect_error "Invoice.n must be unique: AB already exists":
        Invoice(n="AB")
    expect_error "n is too short":
        Invoice(n="A")
    Invoice(n="CD", tag="x")
    expect_error "must be unique":
        Invoice(n="EF", tag="x")
    # per implementor, as in the database (one table each)
    Note(n="AB")
    expect_error "seq must be positive":
        Note(n="GH", seq=0)
    expect_error "n is too short":
        Note(n="G")
"#);
}

#[test]
fn rules_inside_records_are_enforced() {
    run(r#"
record Address:
    street: Text
    zip: Text where z => z.len() == 5
    country: Text default "IT"
    province: Text? required when country == "IT"
    check street.len() > 2, "The street is too short"

record Contact:
    name: Text where n => n.len() > 1
    address: Address?

entity Customer:
    name: Text
    address: Address?
    others: [Address]
    contact: Contact?

test "record rules":
    let ok = Address(street="Via Roma 1", zip="20100", province="MI")
    Customer(name="Anna", address=ok)
    expect_error "Address.zip = 1: does not satisfy its `where`":
        Customer(name="Bea", address=Address(street="Via Roma 1", zip="1", province="MI"))
    expect_error "province is required":
        Customer(name="Bea", address=Address(street="Via Roma 1", zip="20100"))
    Customer(name="Bea", address=Address(street="Rue Neuve 2", zip="75001", country="FR"))
    expect_error "The street is too short":
        Customer(name="Bea", address=Address(street="V", zip="20100", province="MI"))
    expect_error "does not satisfy its `where`":
        Customer(name="Bea", others=[ok, Address(street="Via Roma 1", zip="123", province="MI")])
    expect_error "Address.zip = 9: does not satisfy its `where`":
        Customer(name="Bea", contact=Contact(name="Bea", address=Address(street="Via Roma 1", zip="9", province="MI")))
    let c = Customer(name="Carla")
    expect_error "does not satisfy its `where`":
        c.address = Address(street="Via Roma 1", zip="1", province="MI")
    expect c.address is none
"#);
}

/// `0` is the zero of the type it stands for: a price compares with it, `default 0` stores
/// `0.00 EUR/pcs`, and an empty sum of amounts is `0.00 EUR`.
#[test]
fn zero_takes_the_type_of_its_field_or_expression() {
    run(r#"
entity Product:
    name: Text
    uom: Uom default pcs
    price: Price<EUR, pcs> where > 0
    list: Price<EUR, pcs> default 0
    cost: Money<EUR> default 0
    weight: Qty<kg> default 0
    stock: Qty<uom> default 0
    lines: [Line]
    total = lines.sum(l => l.amount)
    qty_total = lines.sum(l => l.qty)
    shipping: Money<EUR> = when:
        lines.count() == 0 => 0
        else => 4.90 EUR

entity Line:
    product: Product parent
    amount: Money<EUR>
    qty: Qty<pcs>

test "zero":
    let p = Product(name="Pen", price=7.50 EUR/pcs)
    expect p.price > 0
    expect p.price >= 7.5 EUR/pcs
    expect p.list == 0
    expect p.list == 0 EUR/pcs
    expect f"{p.list}" == "0.00 EUR/pcs"
    expect f"{p.cost}" == "0.00 EUR"
    expect f"{p.weight}" == "0 kg"
    expect f"{p.stock}" == "0 pcs"
    expect f"{p.total}" == "0.00 EUR"
    expect f"{p.qty_total}" == "0 pcs"
    expect f"{p.shipping}" == "0.00 EUR"
    expect f"{p.lines.sum(l => l.amount)}" == "0.00 EUR"
    expect p.total == 0 EUR
    expect_error "Product.price = 0.00 EUR/pcs: does not satisfy its `where`":
        Product(name="Bad", price=0 EUR/pcs)
    p.cost = 0
    expect f"{p.cost}" == "0.00 EUR"
"#);
}

/// A quantity in a unit of another kind is refused where the unit is only known at run time
/// (`Qty<uom>`); one of the same kind is accepted.
#[test]
fn quantities_of_another_kind_are_refused() {
    run(r#"
entity Product:
    name: Text
    uom: Uom default pcs
    min_order: Qty<uom>?
    price: Price<EUR, uom>?

test "units":
    let pen = Product(name="Pen")
    expect_error "Product.min_order = 2 kg: expected a quantity in pcs":
        pen.min_order = 2 kg
    expect_error "expected a quantity in pcs":
        Product(name="Ink", min_order=3 l)
    expect_error "Product.price = 2.00 EUR/kg: expected a price per pcs":
        pen.price = 2 EUR/kg
    pen.min_order = 5 pcs
    let flour = Product(name="Flour", min_order=500 g, uom=kg)
    expect flour.min_order == 0.5 kg
    flour.uom = pcs
    expect flour.min_order == 500 g
"#);
}

/// `extend` carries every member it accepts: `unique (…)`, `post to`, a `flow` on a field
/// that has none, `frozen when` on an entity that has none.
#[test]
fn extend_carries_uniques_posts_flows_and_frozen() {
    run_files(&[
        (
            "base.qd",
            r#"
entity Item:
    code: Code
    site: Code
    qty: Int default 1
    state: enum(Draft, Done) default Draft
    note: Text?
"#,
        ),
        (
            "more.qd",
            r#"
from base import Item

ledger Moves:
    key code: Code
    measure qty: Int

extend Item:
    unique (code, site)
    flow state:
        Draft -> Done by finish
    action finish:
        note = "finished"
    frozen when state is Done except note
    post to Moves when state is Done:
        add(code=code, qty=qty)

test "extend":
    let a = Item(code="A", site="X")
    expect_error "unique (code, site)":
        Item(code="A", site="X")
    Item(code="A", site="Y")
    expect Moves.all().count() == 0
    a.finish()
    expect a.state is Done
    expect a.note == "finished"
    expect Moves.all().sum(m => m.qty) == 1
    expect_error "frozen":
        a.qty = 5
    a.note = "still editable"
"#,
        ),
    ]);
}

#[test]
fn extend_refuses_what_it_cannot_merge() {
    let base = r#"
entity Item:
    code: Code
    state: enum(Draft, Done) default Draft
    flow state:
        Draft -> Done by finish
    action finish:
        code = code
    frozen when state is Done
"#;
    assert_error(&[("base.qd", base), ("more.qd", "from base import Item\n\nextend Item:\n    flow state:\n        Done -> Draft by reopen\n")], "`Item.state` already has a flow");
    assert_error(
        &[
            ("base.qd", base),
            (
                "more.qd",
                "from base import Item\n\nextend Item:\n    frozen when code == \"X\"\n",
            ),
        ],
        "`Item` already has `frozen when`",
    );
    assert_error(
        &[
            ("base.qd", base),
            (
                "more.qd",
                "from base import Item\n\nextend Item:\n    load from \"items.csv\"\n",
            ),
        ],
        "`load from` cannot be used in an `extend`",
    );
    assert_error(
        &[(
            "main.qd",
            "entity Item:\n    code: Code\n    load from \"items.csv\"\n",
        )],
        "`load from` is only for a `temporary entity`",
    );
}

/// An entity of the standard library is extended, not redeclared.
#[test]
fn std_entities_cannot_be_redeclared() {
    assert_error(
        &[(
            "main.qd",
            "entity Company:\n    name: Text\n    vat: Text\n",
        )],
        "use `extend Company:`",
    );
    assert!(errors(&[("main.qd", "extend Company:\n    vat: Text?\n")]).is_empty());
}

/// A plain `check` in a ledger holds on every row a record posts.
#[test]
fn plain_ledger_checks_hold_on_every_posted_row() {
    run(r#"
ledger Moves:
    key code: Code
    measure qty: Int
    check qty != 0, "A move of nothing"
    check code.len() == 3, f"Bad code {code}"

entity Item:
    code: Code
    qty: Int
    state: enum(Draft, Done, Cancelled) default Draft
    post to Moves when state is Done:
        add(code=code, qty=qty)

test "row checks":
    let a = Item(code="ABC", qty=2)
    a.state = Done
    expect Moves.all().sum(m => m.qty) == 2
    let b = Item(code="ABCD", qty=1)
    expect_error "Bad code ABCD":
        b.state = Done
    expect b.state is Draft
    let c = Item(code="XYZ", qty=0)
    expect_error "A move of nothing":
        c.state = Done
    # the reversal of a checked row is not checked again
    a.state = Cancelled
    expect Moves.all().sum(m => m.qty) == 0
"#);
    assert_error(&[("main.qd", "ledger Moves:\n    key code: Code\n    measure qty: Int\n    warn qty < 100, \"big\"\n")], "a ledger row cannot carry a warning");
}

/// `on X created` runs once for each record created in a transaction, `on X changes` once
/// for each existing record written in it.
#[test]
fn created_and_changes_reactions_run() {
    run(r#"
entity Item:
    code: Code
    qty: Int default 0
    edits: Int default 0

entity Audit:
    item: Item
    what: Text

on Item created as i:
    Audit(item=i, what="created")

on Item changes as i:
    Audit(item=i, what=f"changed {i.qty}")
    i.edits += 1

test "created and changes":
    let a = Item(code="A")
    expect Audit.where(x => x.what == "created").count() == 1
    expect Audit.where(x => x.what != "created").count() == 0
    expect a.edits == 0
    a.qty = 3
    expect Audit.where(x => x.what == "changed 3").count() == 1
    expect a.edits == 1
    a.qty = 4
    expect Audit.all().count() == 3
    expect a.edits == 2
"#);
}

#[test]
fn deleted_reactions_are_refused() {
    assert_error(
        &[(
            "main.qd",
            "entity Item:\n    code: Code\n\non Item deleted as i:\n    fail \"no\"\n",
        )],
        "records are not deleted in Quadra",
    );
}

/// A `becomes` reaction whose effects change what its condition reads must be declared
/// `converges`; `converges` belongs to `becomes` reactions only.
#[test]
fn self_triggering_reactions_need_converges() {
    let model = r#"
entity Rule:
    min: Int
    lines: [Line]
    have = lines.sum(l => l.qty)
    to_order = min - have if have < min else 0

entity Line:
    rule: Rule parent
    qty: Int
"#;
    let without =
        format!("{model}\non Rule becomes to_order > 0 as r:\n    Line(rule=r, qty=r.to_order)\n");
    assert_error(&[("main.qd", &without)], "declare it `converges`");
    let with =
        format!("{model}\non Rule becomes to_order > 0 as r converges:\n    Line(rule=r, qty=r.to_order)\n\ntest \"reorder\":\n    let r = Rule(min=5)\n    expect r.lines.count() == 1\n    expect r.to_order == 0\n");
    run(&with);
    let enters =
        "entity Doc:\n    state: enum(Draft, Done) default Draft\n    flow state:\n        Draft -> Done by finish\n    action finish:\n        require true\n\non Doc enters Done as d converges:\n    require true\n";
    assert_error(
        &[("main.qd", enters)],
        "`converges` applies only to `becomes` reactions",
    );
}

/// Permissions: `deny` inside a role applies to its users, a top-level `allow` grants to
/// everybody, `show` undoes a `hide` of another role; `delete` and `derived` are refused.
#[test]
fn deny_in_roles_top_level_allow_and_show() {
    let src = r#"
entity Doc:
    title: Text
    locked: Bool default false
    cost: Int default 0

entity Note:
    text: Text

role Clerk:
    read, create, edit Doc
    deny edit Doc.title where d => d.locked
    hide Doc.cost

role Senior is Clerk:
    show Doc.cost

role Buyer:
    read Doc
    show Doc.cost

allow create Note

test "rules":
    User(login="ann", roles={Clerk})
    User(login="sam", roles={Senior})
    User(login="bob", roles={Clerk, Buyer})
    let d = as ann: Doc(title="a")
    as ann:
        d.title = "b"
    d.locked = true
    expect_error "permission denied: deny edit Doc.title":
        as ann: d.title = "c"
    expect_error "permission denied: deny edit Doc.title":
        as sam: d.title = "c"
    as ann:
        d.cost = 1
    as ann: Note(text="anyone may write a note")
    as bob: Note(text="me too")
"#;
    run(src);
    let p = project(&[("main.qd", src)]);
    let hidden = quadra_runtime::run_session(&p, "rules", |_, _, interp, _| {
        let mut out = Vec::new();
        for login in ["ann", "sam", "bob"] {
            let id = interp.ids("User", None).into_iter().find(|id| {
                interp
                    .store
                    .read(*id, "login", None)
                    .map_or(false, |v| v.to_string() == login)
            });
            interp.user = id;
            out.push((login, interp.hidden("Doc", "cost")));
        }
        out
    })
    .unwrap_or_else(|_| panic!("does not check"));
    assert_eq!(hidden.0, None, "the test fails");
    assert_eq!(
        hidden.1,
        vec![("ann", true), ("sam", false), ("bob", false)]
    );
    let refused = |rule: &str, expected: &str| {
        let src = format!("entity Doc:\n    title: Text\n\nrole Clerk:\n    {rule}\n");
        assert_error(&[("main.qd", &src)], expected);
    };
    refused("delete Doc", "records are not deleted in Quadra");
    refused("read, derived Doc", "`derived` is not a permission");
    refused("hide, read Doc.title", "`hide` cannot be combined");
    assert_error(
        &[(
            "main.qd",
            "entity Doc:\n    title: Text\n\nallow hide Doc.title\n",
        )],
        "inside a `role`",
    );
}

// ---- frozen documents and flows (items 15–18) ---------------------------------------------

const FLOW_ORDER: &str = r#"
entity Order:
    n: Int default 0
    state: enum(Draft, Confirmed, Shipped) default Draft
    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Shipped by ship
"#;

/// 15. The field of a flow changes only through its transitions: assigning it is a compile
/// error in code and refused at run time, except in a `change` (a data fix).
#[test]
fn a_flow_field_is_not_assigned() {
    let msg = "`Order.state` changes only through its flow: run one of its actions (confirm, ship)";
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}    action force:\n        state = Shipped\n"),
        )],
        msg,
    );
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}    action force:\n        self.state = Shipped\n"),
        )],
        msg,
    );
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}\ntest \"t\":\n    let o = Order()\n    o.state = Shipped\n"),
        )],
        msg,
    );
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}\non Order enters Confirmed as o:\n    o.state = Shipped\n"),
        )],
        msg,
    );
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}\nfixture f:\n    let o = Order()\n    o.state = Shipped\n"),
        )],
        msg,
    );
    // a new record starts in the initial state
    assert_error(
        &[(
            "main.qd",
            &format!("{FLOW_ORDER}\ntest \"t\":\n    let o = Order(state=Shipped)\n"),
        )],
        "a new Order starts in Draft",
    );
    assert!(errors(&[(
        "main.qd",
        &format!("{FLOW_ORDER}\ntest \"t\":\n    let o = Order(state=Draft)\n    o.n = 1\n")
    )])
    .is_empty());
    // only automatic transitions
    let auto = "entity Task:\n    n: Int default 0\n    state: enum(Open, Big) default Open\n    flow state:\n        Open -> Big when n > 10\n    action force:\n        state = Big\n";
    assert_error(
        &[("main.qd", auto)],
        "`Task.state` changes only through its flow: it moves by its automatic transitions",
    );

    // at run time (the API's `update`, any other path): refused with the actions that move it
    let src = format!("{FLOW_ORDER}\nchange fix_state:\n    reason: \"wrongly confirmed\"\n    for o in Order.all():\n        o.state = Draft\n\ntest \"t\":\n    let o = Order()\n    o.confirm()\n");
    let p = project(&[("main.qd", &src)]);
    let (outcome, ()) = quadra_runtime::run_session(&p, "t", |m, _, interp, vars| {
        let o = vars["o"].clone();
        let shipped = quadra_runtime::V::Variant {
            name: "Shipped".into(),
            fields: Default::default(),
        };
        let err = interp
            .transaction(|i| i.set(&o, "state", shipped.clone(), None))
            .unwrap_err();
        assert!(format!("{err:?}").contains(msg), "{err:?}");
        let err = interp
            .transaction(|i| i.create("Order", vec![("state".into(), shipped.clone())]))
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("a new Order starts in Draft"),
            "{err:?}"
        );
        assert_eq!(
            interp
                .get(&o, "state", None, None)
                .ok()
                .map(|v| v.to_string()),
            Some("Confirmed".into())
        );
        // a change (a data fix, with a reason and an approval) may set it
        let change = m
            .changes
            .iter()
            .find(|c| c.name == "fix_state")
            .unwrap()
            .clone();
        interp
            .transaction(|i| i.run_change(&change))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            interp
                .get(&o, "state", None, None)
                .ok()
                .map(|v| v.to_string()),
            Some("Draft".into())
        );
    })
    .unwrap_or_else(|d| panic!("does not check: {d:#?}"));
    assert_eq!(outcome, None);
}

const REOPEN: &str = r#"
entity Customer:
    name: Text

entity Order:
    customer: Customer
    notes: Text default ""
    reopened: Int default 0
    lines: [OrderLine]
    state: enum(Draft, Confirmed, Locked) default Draft
    customer_name = customer.name
    total = lines.sum(l => l.qty)
    frozen when state is not Draft
    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Locked by lock
        Confirmed -> Draft by reopen
    action reopen:
        reopened += 1
        notes = "reopened"
    action lock:
        notes = "locked"

entity OrderLine:
    order: Order parent
    qty: Int

on Order enters Draft as o:
    o.notes = f"{o.notes}, back to draft"
"#;

/// 16. A record is frozen while `frozen when` holds: a transition that makes it false makes
/// it (and its lines) editable again, and the action that leaves the frozen state may write.
#[test]
fn a_transition_out_of_the_frozen_states_unfreezes() {
    let src = format!(
        "{REOPEN}{}",
        r#"
test "reopen":
    let c = Customer(name="Anna")
    let o = Order(customer=c, lines=[OrderLine(qty=1)])
    let l = o.lines.first() ?? fail "no line"
    o.confirm()
    expect_error "Order is frozen (state is not Draft): `notes` cannot change":
        o.notes = "x"
    expect_error "Order is frozen (state is not Draft): its OrderLine lines cannot change":
        l.qty = 2
    c.name = "Anna B."
    expect o.customer_name == "Anna"
    o.reopen()
    expect o.state is Draft
    expect o.reopened == 1
    expect o.notes == "reopened, back to draft"
    expect o.customer_name == "Anna B."
    l.qty = 2
    o.notes = "edited"
    expect o.total == 2
    o.confirm()
    expect_error "its OrderLine lines cannot change":
        l.qty = 3
    # a transition between frozen states does not unfreeze: its body may not write
    expect_error "Order is frozen (state is not Draft): `notes` cannot change":
        o.lock()
    expect o.state is Confirmed
"#
    );
    run(&src);
}

const LINES: &str = r#"
entity Product:
    name: Text
    price: Int
    active: Bool default true

entity Order:
    lines: [OrderLine]
    state: enum(Draft, Confirmed) default Draft
    total = lines.sum(l => l.amount)
    frozen when state is not Draft
    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Draft by reopen

entity OrderLine:
    order: Order parent
    product: Product where p => p.active
    qty: Int
    amount = qty * product.price
    current_price = product.price
    live current_price

extend OrderLine:
    product_name = product.name
"#;

/// 17. The lines of a frozen document are frozen with it: their formulas read the data as of
/// the moment the document was frozen, except those declared `live` on the line.
#[test]
fn lines_are_frozen_with_their_document() {
    let src = format!(
        "{LINES}{}",
        r#"
test "lines":
    let p = Product(name="Pen", price=2)
    let o = Order(lines=[OrderLine(product=p, qty=3)])
    let l = o.lines.first() ?? fail "no line"
    o.confirm()
    p.name = "Blue pen"
    p.price = 5
    expect l.product_name == "Pen"
    expect l.amount == 6
    expect o.total == 6
    expect l.current_price == 5
    o.reopen()
    expect l.product_name == "Blue pen"
    expect l.amount == 15
    expect o.total == 15
"#
    );
    run(&src);
    // `live` on a line needs a frozen document
    assert_error(
        &[("main.qd", "entity Order:\n    lines: [OrderLine]\n\nentity OrderLine:\n    order: Order parent\n    qty: Int\n    q2 = qty * 2\n    live q2\n")],
        "`live q2`: OrderLine is never frozen (it has no `frozen when`, and it is not a line of a document that has one)",
    );
}

/// 18. The constraints of a frozen record were satisfied when it was frozen: a reference's
/// `where` is not checked again against later data.
#[test]
fn a_frozen_records_where_is_not_checked_again() {
    let src = format!(
        "{LINES}{}",
        r#"
test "where":
    let p = Product(name="Pen", price=2)
    let o = Order(lines=[OrderLine(product=p, qty=3)])
    o.confirm()
    p.active = false
    expect p.active == false
    # a draft order still refuses it
    expect_error "OrderLine.product = ":
        Order(lines=[OrderLine(product=p, qty=1)])
    # reopened, the order is checked again
    expect_error "OrderLine.product = ":
        o.reopen()
"#
    );
    run(&src);
    // a frozen record's own `where`
    run(r#"
entity Product:
    active: Bool default true

entity Order:
    product: Product where p => p.active
    state: enum(Draft, Done) default Draft
    frozen when state is Done
    flow state:
        Draft -> Done by done

test "where":
    let p = Product()
    let o = Order(product=p)
    o.done()
    p.active = false
"#);
}

// ---- items 19–36 --------------------------------------------------------------------------

/// 19–21. `else if` on one line, a statement keyword before `(`, `??` before `==`.
#[test]
fn else_if_keyword_parenthesis_and_coalesce() {
    run(r#"
fn sign(x: Int) -> Int:
    if x > 0:
        return 1
    else if x < 0:
        return -1
    else:
        return 0

test "syntax":
    expect sign(3) == 1
    expect sign(-3) == -1
    expect sign(0) == 0
    expect (sign(1) + 1) == 2
    expect [1, 2].count() == 2
    let a: Int? = none
    expect a ?? 1 == 1
    let b: Int? = 5
    expect b ?? 1 == 5
    expect not (b ?? 0 == 1)
"#);
}

/// 22. What the checker accepts the engine runs; the rest is a compile error.
#[test]
fn date_helpers_updated_at_payload_variants_and_as_percent() {
    run(r#"
entity Item:
    code: Code
    qty: Int default 0
    rule: enum(Fixed(amount: Money<EUR>), Discount(off: Percent), Free) default Free

    fn price(list: Money<EUR>) -> Exact<Money<EUR>>:
        return match rule:
            Fixed(amount) => amount
            Discount(off) => list * (1 - off)
            Free => 0 EUR

test "helpers":
    expect 2026-11-21.start_of_month() == 2026-11-01
    expect 2026-02-10.end_of_month() == 2026-02-28
    expect 2026-11-21 10:30.start_of_month() == 2026-11-01 00:00
    expect 2026-11-21 10:30.end_of_month() == 2026-11-30 23:59:59
    expect 3.as_percent() == 300%
    expect 0.25.as_percent() == 25%
    expect 25%.as_percent() == 25%

test "updated_at and updated_by":
    clock 2026-11-02 09:00
    User(login="ann")
    let i = Item(code="A")
    expect i.updated_at == 2026-11-02 09:00
    expect i.updated_at == i.created_at
    clock 2026-11-03 10:00
    login("ann")
    i.qty = 2
    expect i.updated_at == 2026-11-03 10:00
    expect i.updated_by?.login == "ann"
    expect i.created_at == 2026-11-02 09:00

test "payload variants":
    let i = Item(code="B", rule=Discount(off=10%))
    expect i.price(10.00 EUR) == 9.00 EUR
    i.rule = Fixed(amount=7.00 EUR)
    expect i.price(10.00 EUR) == 7.00 EUR
    i.rule = Fixed(8.00 EUR)
    expect i.price(10.00 EUR) == 8.00 EUR
    expect f"{i.rule}" == "Fixed(8.00 EUR)"
"#);
    let item = "entity Item:\n    code: Code\n    rule: enum(Fixed(amount: Money<EUR>), Free) default Free\n\n";
    for (body, expected) in [
        (
            "fn f(i: Item) -> Item:\n    return i.as_of(2026-01-01)\n",
            "`as_of` is not available",
        ),
        (
            "fn f(i: Item) -> Text:\n    return i.explain()\n",
            "`explain` is not available",
        ),
        (
            "fn f(i: Item) -> Int:\n    return i.history.count()\n",
            "`history` is not available",
        ),
        (
            "fn f(i: Item):\n    i.rule = Fixed(amount=1.00 EUR, extra=1)\n",
            "`Fixed` has no field `extra`",
        ),
        (
            "fn f(i: Item):\n    i.rule = Free(1)\n",
            "`Free` has no fields",
        ),
        (
            "fn f(i: Item):\n    i.rule = Fixed()\n",
            "`Fixed` needs `amount`",
        ),
    ] {
        assert_error(&[("main.qd", &format!("{item}{body}"))], expected);
    }
}

/// 23. f-strings: format specs, `{{` `}}` escapes, `\n` `\t` `\"` escapes.
#[test]
fn fstring_format_specs_and_escapes() {
    run(r#"
test "f-strings":
    let n = 42
    expect f"{n:05}" == "00042"
    expect f"{n:5}" == "   42"
    expect f"{n:<5}|" == "42   |"
    expect f"{n:^6}|" == "  42  |"
    expect f"{-7:04}" == "-007"
    let x = 3.14159
    expect f"{x:.2}" == "3.14"
    expect f"{x:08.3}" == "0003.142"
    expect f"{n:.1}" == "42.0"
    expect f"{2.5 kg:.2}" == "2.50 kg"
    expect f"{12.5 EUR:.0}" == "12 EUR"
    let t = "ab"
    expect f"{t:4}|" == "ab  |"
    expect f"{t:>4}|" == "  ab|"
    expect f"{{n}} = {n}" == "{n} = 42"
    expect f"a}}b" == "a}b"
    expect f"a\nb" == "a\nb"
    expect f"a\nb".len() == 3
    expect f"a\tb".len() == 3
    expect f"\"q\"".len() == 3
    expect f"a\\b".len() == 3
    expect "x\ty".len() == 3
    expect "a\\nb".len() == 4
    expect f"{n} \"q\"" == "42 \"q\""
"#);
    for (src, expected) in [
        ("const A = f\"{1:xyz}\"\n", "invalid format `xyz`"),
        (
            "fn f(t: Text) -> Text:\n    return f\"{t:.2}\"\n",
            "`.2` applies to numbers",
        ),
        (
            "fn f(t: Text) -> Text:\n    return f\"{t:05}\"\n",
            "`0` padding applies to numbers",
        ),
        (
            "fn f(m: Money<EUR>) -> Text:\n    return f\"{m:010}\"\n",
            "`0` padding applies to numbers",
        ),
        ("const A = f\"{1 +}\"\n", "invalid expression in f-string"),
    ] {
        assert_error(&[("main.qd", src)], expected);
    }
}

/// 25. A product by an integer keeps the exactness of the operand, for prices as for money
/// and quantities.
#[test]
fn product_by_an_integer_is_not_exact() {
    run(r#"
entity Line:
    price: Price<EUR, pcs>
    double: Price<EUR, pcs> default price * 2
    total: Money<EUR> default 3 * 1.50 EUR

test "exactness":
    let l = Line(price=1.25 EUR/pcs)
    expect l.double == 2.50 EUR/pcs
    expect l.total == 4.50 EUR
"#);
    assert_error(&[("main.qd", "entity Line:\n    price: Price<EUR, pcs>\n    half: Price<EUR, pcs> default price * 0.5\n")], "must be rounded");
}

/// 31. `.round()` uses the rounding mode of the currency.
#[test]
fn money_rounds_with_the_currency_rounding_mode() {
    run(r#"
test "rounding modes":
    expect (10.005 EUR * 1).round() == 10.00 EUR
    expect (10.005 EUR * 1.0).round() == 10.00 EUR
    EUR.rounding = HalfUp
    expect (10.005 EUR * 1.0).round() == 10.01 EUR
    expect (-10.005 EUR * 1.0).round() == -10.01 EUR
    EUR.rounding = Up
    expect (10.001 EUR * 1.0).round() == 10.01 EUR
    EUR.rounding = Down
    expect (10.009 EUR * 1.0).round() == 10.00 EUR
    expect (10.07 EUR * 1.0).round(step=0.05) == 10.05 EUR
    expect (1.23456 EUR/pcs * 1.0).round() == 1.2345 EUR/pcs
"#);
}

/// 31. Every sequence is gapless: a number taken by a transaction that fails is given again.
#[test]
fn a_failed_transaction_leaves_no_gap_in_a_sequence() {
    run(r#"
entity Invoice:
    number: Code default next_number(current.company, "INV", on=2026-01-10)
    amount: Int
    check amount > 0, "empty invoice"

test "gapless":
    let c = Company(name="Acme", currency=EUR)
    login("admin", company=c)
    Sequence(code="INV", pattern="INV/{year}/{n:03}")
    let a = Invoice(amount=1)
    expect a.number == "INV/2026/001"
    expect_error "empty invoice":
        Invoice(amount=0)
    let b = Invoice(amount=2)
    expect b.number == "INV/2026/002"
"#);
}

/// 33. Rows posted earlier in a transaction are read later in the same transaction.
#[test]
fn rows_posted_in_a_transaction_are_visible_in_it() {
    run(r#"
ledger Stock:
    key code: Code
    measure qty: Int

entity Receipt:
    code: Code
    qty: Int
    post to Stock:
        add(code=code, qty=qty)

entity Order:
    code: Code
    qty: Int
    state: enum(Draft, Confirmed) default Draft
    flow state:
        Draft -> Confirmed by confirm
    available = Stock.where(s => s.code == code).sum(s => s.qty)

before Order.confirm as o:
    require o.available >= o.qty, f"only {o.available} available"

fixture stocked:
    Receipt(code="A", qty=5)
    let o = Order(code="A", qty=3)
    o.confirm()

test "same transaction":
    use stocked
    expect Order.all().count(o => o.state is Confirmed) == 1
    expect Stock.all().sum(s => s.qty) == 5
"#);
}

/// 34. Prices show the decimals of their currency, like amounts.
#[test]
fn prices_display_with_the_currency_decimals() {
    run(r#"
test "display":
    expect f"{7.5 EUR/pcs}" == "7.50 EUR/pcs"
    expect f"{1.2345 EUR/pcs}" == "1.2345 EUR/pcs"
    expect f"{7 EUR/pcs}" == "7.00 EUR/pcs"
    expect f"{500 JPY/pcs}" == "500 JPY/pcs"
    expect f"{500 JPY}" == "500 JPY"
    expect f"{7.5 EUR}" == "7.50 EUR"
"#);
}

/// 35. A single-field `unique (n)` gives the same message in memory and on a database.
#[test]
fn single_field_unique_message() {
    run(r#"
entity Invoice:
    n: Code
    unique (n)

test "unique":
    Invoice(n="AB")
    expect_error "Invoice.n must be unique: AB already exists":
        Invoice(n="AB")
"#);
}

/// 36. A quantity in another unit of the same kind is stored in the field's unit.
#[test]
fn quantities_are_stored_in_the_field_unit() {
    run(r#"
entity Product:
    code: Code
    weight: Qty<kg>
    uom: Uom default kg
    stock: Qty<uom> default 0

test "conversion":
    let p = Product(code="A", weight=500 g)
    expect f"{p.weight}" == "0.5 kg"
    p.weight = 2 t
    expect f"{p.weight}" == "2000 kg"
    p.stock = 1500 g
    expect f"{p.stock}" == "1.5 kg"
"#);
}

/// Durations in hours and minutes: `hour(s)` and `minute(s)` are words of the language like
/// `day(s)`, and a duration shows as people read it.
#[test]
fn durations_take_hours_and_minutes() {
    run(r#"
entity TimeLog:
    hours: Duration
    date: Date

test "hours":
    let t = TimeLog(hours=8 hours, date=2026-11-20)
    expect f"{t.hours}" == "8 h"
    t.hours = 1 hour + 30 minutes
    expect f"{t.hours}" == "1 h 30 min"
    expect f"{45 minutes}" == "45 min"
    expect f"{1 day}" == "1 day"
    expect f"{2 weeks}" == "14 days"
    expect f"{26 hours}" == "26 h"
    expect (90 minutes).to(h) == 1.5 h
    expect t.date + 2 days == 2026-11-22
"#);
}

/// A duration compares with 0 (`check hours > 0`) like money and quantities do; with any other
/// bare number it is a compile error (5 what?).
#[test]
fn durations_compare_with_zero() {
    run(r#"
entity TimeLog:
    hours: Duration
    check hours > 0, "Hours must be greater than 0"
    check hours <= 12 hours, "At most 12 hours"

test "zero":
    let t = TimeLog(hours=1 hour)
    expect t.hours > 0
    expect t.hours != 0
    expect t.hours + 0 == 1 hour
    expect (t.hours - t.hours) == 0
    expect_error "Hours must be greater than 0":
        TimeLog(hours=0 hours)
    expect_error "At most 12 hours":
        TimeLog(hours=13 hours)
"#);
    assert_error(
        &[(
            "main.qd",
            "entity TimeLog:\n    hours: Duration\n    check hours > 5, \"five\"\n",
        )],
        "cannot compare Duration and Int: write the unit, as in `5 hours`, `30 minutes`, `2 days`",
    );
}

#[test]
fn group_by_two_keys_avg_and_first_n() {
    run(r#"
entity Region:
    name: Text

entity Sale:
    region: Region
    kind: enum(Goods, Services)
    date: Date
    amount: Money<EUR>
    qty: Int?

test "group_by with one and two keys, aggregates, first(n)":
    let n = Region(name="North")
    let s = Region(name="South")
    Sale(region=n, kind=Goods, date=2026-01-05, amount=10.00 EUR, qty=1)
    Sale(region=s, kind=Goods, date=2026-01-20, amount=5.00 EUR, qty=none)
    Sale(region=n, kind=Services, date=2026-02-01, amount=20.00 EUR, qty=3)
    Sale(region=n, kind=Goods, date=2026-02-11, amount=30.00 EUR, qty=4)
    let by_region = Sale.group_by(x => x.region).map(g => (r: g.key.name, n: g.items.count(), t: g.items.sum(x => x.amount)))
    expect by_region == [(r: "North", n: 3, t: 60.00 EUR), (r: "South", n: 1, t: 5.00 EUR)]
    let two = Sale.group_by(x => x.region, x => x.date.month).map(g => (g.key.name, g.key2, g.items.count()))
    expect two.count() == 3
    expect two.first() == ("North", 2026-01, 1)
    expect Sale.all().avg(x => x.amount) == 16.25 EUR
    expect Sale.all().avg(x => x.qty) == 2.6666666666666666666666666667
    expect [1, 2, 3, 4].avg() == 2.5
    expect [].avg(x => x) == none
    expect [3, 1, 2].first(2) == [3, 1]
    expect [3, 1, 2].last(2) == [1, 2]
    expect [3, none, 1].max() == 3
    expect Sale.group_by(x => x.kind).sort_by(g => -g.items.count()).first(1).map(g => g.key) == [Goods]
    expect Sale.group_by(x => x.region, x => x.date.month).sort_by(g => -g.items.sum(x => x.amount).raw).first(1).map(g => (region: g.key.name, month: g.key2, total: g.items.sum(x => x.amount))) == [(region: "North", month: 2026-02, total: 50.00 EUR)]
"#);
}

#[test]
fn group_by_checks_its_keys() {
    assert_error(&[("main.qd", "entity A:\n    n: Int\n\ntest \"x\":\n    let g = A.group_by(a => a.n, a => a.n, a => a.n)\n")], "group_by takes one or two keys");
    assert_error(
        &[(
            "main.qd",
            "entity A:\n    t: Text\n\ntest \"x\":\n    let g = A.all().avg(a => a.t)\n",
        )],
        "cannot average",
    );
    assert_error(
        &[("main.qd", "test \"x\":\n    let g = [1, 2].first(\"a\")\n")],
        "takes a whole number",
    );
}

/// A range of billions is refused before it is made: one expression cannot take the machine's
/// memory.
#[test]
fn a_huge_range_is_refused_not_allocated() {
    let project = project(&[(
        "main.qd",
        "test \"huge\":\n    let n = (1..9000000000000).count()\n    expect n > 0\n",
    )]);
    let started = std::time::Instant::now();
    let results = quadra_runtime::run_tests(&project, None).unwrap();
    assert!(!results[0].passed);
    let m = results[0].message.clone().unwrap_or_default();
    assert!(
        m.contains("elements") || m.contains("list") || m.contains("long"),
        "{m}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// Integers that overflow fail instead of wrapping into a wrong value.
#[test]
fn an_integer_overflow_is_an_error() {
    let project = project(&[("main.qd", "test \"overflow\":\n    let big = 9223372036854775807\n    let n = big + 1\n    expect n > 0\n")]);
    let results = quadra_runtime::run_tests(&project, None).unwrap();
    assert!(!results[0].passed);
    assert!(
        results[0]
            .message
            .clone()
            .unwrap_or_default()
            .contains("too large"),
        "{:?}",
        results[0].message
    );
}
