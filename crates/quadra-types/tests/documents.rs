//! Documents and labels of `.qdp` files: what the checker says about them.

use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

const MODEL: &str = r#"
entity Customer:
    name: Text

entity Invoice:
    number: Text?
    customer: Customer
    total: Decimal
    state: enum(Draft, Posted) default Draft
    pdf: File?
    lines: [Line]

    frozen when state is Posted

    action post:
        number = "INV-1"

entity Line:
    invoice: Invoice parent
    what: Text
    amount: Decimal
"#;

/// The errors of a project: `model.qd`, and `documents/doc.qdp` with `qdp`.
fn errors(qdp: &str, more_qd: &str) -> Vec<String> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/__documents");
    let sources = vec![
        (root.join("model.qd"), format!("{MODEL}\n{more_qd}")),
        (root.join("documents/doc.qdp"), qdp.to_string()),
    ];
    let project = Project::from_sources(&root, sources);
    let analysis = project.analyze();
    let mut out: Vec<String> = analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    let model = quadra_ir::lower(&project, &analysis);
    out.extend(model.errors.iter().map(|(_, m)| m.clone()));
    out.extend(
        quadra_types::check_model(&model)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message),
    );
    out
}

fn doc(body: &str) -> String {
    format!(
        "from model import Invoice\n\ndocument D(inv: Invoice):\n{}",
        body.lines()
            .map(|l| format!("    {l}\n"))
            .collect::<String>()
    )
}

fn assert_error(qdp: &str, expected: &str) {
    let e = errors(qdp, "");
    assert!(
        e.iter().any(|m| m.contains(expected)),
        "{qdp}\nexpected {expected:?}, got {e:#?}"
    );
}

#[test]
fn a_good_document_has_no_errors() {
    let e = errors(&doc("title \"Invoice\"\npage A4 margin 18\nheading inv.customer.name level 1\ntable inv.lines as l:\n    column \"What\" width 3: l.what\n    column \"Amount\" align right: l.amount\ntotals:\n    item \"Total\" bold: inv.total\nbarcode inv.number ?? \"\" type qr height 20"), "");
    assert!(e.is_empty(), "{e:#?}");
}

#[test]
fn fields_are_checked_with_the_model() {
    assert_error(&doc("text inv.totale"), "totale");
}

#[test]
fn elements_and_options_are_checked() {
    assert_error(
        &doc("paragraph \"x\""),
        "a document has no element `paragraph`",
    );
    assert_error(&doc("text \"x\" level 2"), "`text` has no option `level`");
    assert_error(
        &doc("text \"x\" align middle"),
        "`align` is left, center, right",
    );
    assert_error(
        &doc("table inv.lines:\n    column \"What\": l.what"),
        "a table names its rows",
    );
    assert_error(&doc("barcode \"x\""), "a barcode says its kind");
    assert_error(&doc("page B9"), "the size of the page");
    assert_error(
        &doc("image \"/etc/passwd.png\""),
        "the path of an image is in the project",
    );
    assert_error(
        &doc("table inv.lines as l:\n    text \"no\""),
        "a table holds its columns",
    );
}

#[test]
fn a_label_has_a_size_and_places() {
    let qdp = "from model import Invoice\n\nlabel L(inv: Invoice) width 100 height 150 dpi 203:\n    text inv.customer.name x 5 y 5 size 12\n    box x 2 y 2 width 96 height 146\n";
    assert!(errors(qdp, "").is_empty(), "{:#?}", errors(qdp, ""));
    assert_error(
        "from model import Invoice\n\nlabel L(inv: Invoice):\n    text \"x\" x 1 y 1\n",
        "a label says its size",
    );
    assert_error("from model import Invoice\n\nlabel L(inv: Invoice) width 100 height 50 dpi 150:\n    text \"x\" x 1 y 1\n", "`dpi` is the printer's resolution");
    assert_error(
        "from model import Invoice\n\nlabel L(inv: Invoice) width 100 height 50:\n    text \"x\"\n",
        "an element of a label has its place",
    );
}

#[test]
fn render_runs_after_the_commit() {
    let e = errors(&doc("text inv.total"), "from documents.doc import D\n\nextend Invoice:\n    action print_now:\n        let p = render(D(self))\n");
    assert!(
        e.iter().any(|m| m.contains("it runs after the commit")),
        "{e:#?}"
    );
}

#[test]
fn attach_needs_a_file_field_and_the_entity() {
    let base = doc("text inv.total");
    assert!(errors(
        &format!("{base}\nattach D to Invoice.pdf when state is Posted\n"),
        ""
    )
    .is_empty());
    assert_error(
        &format!("{base}\nattach D to Invoice.number when state is Posted\n"),
        "keeps the document: it is a `File?`",
    );
    assert_error(
        &format!("{base}\nattach D to Invoice.nope\n"),
        "`Invoice` has no field `nope`",
    );
    assert_error(
        &format!("{base}\nattach D to Invoice.pdf signed \"company\"\n"),
        "no signer `company`",
    );
}

#[test]
fn documents_live_in_qdp_files() {
    let e = errors(
        "from model import Invoice\n\nentity Nope:\n    x: Int\n",
        "",
    );
    assert!(
        e.iter()
            .any(|m| m.contains("a `.qdp` file holds documents and labels")),
        "{e:#?}"
    );
}
