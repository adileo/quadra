//! Retention: what the checker says about `retain`, `expire`, `personal`, `history` and
//! `erase`.

use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

fn errors(src: &str) -> Vec<String> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/__retention");
    let project = Project::from_sources(&root, vec![(root.join("model.qd"), src.to_string())]);
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

fn assert_error(src: &str, expected: &str) {
    let e = errors(src);
    assert!(
        e.iter().any(|m| m.contains(expected)),
        "{src}\nexpected {expected:?}, got {e:#?}"
    );
}

const OK: &str = r#"
entity Customer:
    name: Text? personal
    email: Email? personal
    notes: Text? expire 30 days after created_at
    vat: Text?

entity Invoice:
    retain 10 years after date, then expire
    customer: Customer
    date: Date
    customer_name: Text? default customer.name
    lines: [Line]

entity Line:
    invoice: Invoice parent
    what: Text

entity Session:
    expire 2 days after created_at
    history 1 year
    token: Text

entity Visit:
    expire 1 year after at
    session: Session
    at: DateTime

entity Request:
    customer: Customer
    action forget:
        erase customer, "asked"
"#;

#[test]
fn a_model_with_retention_checks() {
    assert_eq!(errors(OK), Vec::<String>::new());
}

#[test]
fn an_erasable_field_is_optional() {
    assert_error(
        "entity C:\n    name: Text personal\n",
        "`C.name` is personal: `erase` empties it, so it must be optional (`name: Text?`)",
    );
    assert_error(
        "entity C:\n    note: Text expire 2 days after created_at\n",
        "`C.note` expires: it is emptied then, so it must be optional (`note: Text?`)",
    );
}

#[test]
fn a_term_counts_from_a_date() {
    assert_error(
        "entity C:\n    expire 2 days after name\n    name: Text\n",
        "`C.name` is not a stored `Date` or `DateTime`",
    );
    assert_error(
        "entity C:\n    expire 2 days after born\n    name: Text\n",
        "C has no field `born` to count the time from",
    );
    assert_error(
        "entity C:\n    expire 2 fortnights after created_at\n    name: Text\n",
        "`fortnights` is not a unit of time here: hours, days, weeks, months or years",
    );
}

#[test]
fn expire_is_not_shorter_than_retain() {
    assert_error(
        "entity C:\n    retain 10 years after created_at\n    expire 1 year after created_at\n    name: Text\n",
        "C expires (1 year after created_at) before the end of its retention (10 years after created_at)",
    );
}

#[test]
fn a_record_kept_for_years_does_not_show_what_disappears() {
    assert_error(
        "entity Customer:\n    name: Text? personal\n\nentity Invoice:\n    retain 10 years after created_at\n    customer: Customer\n    shown = customer.name\n",
        "`Invoice.shown` is kept 10 years after created_at, but `Customer.name` is personal: `erase` empties it",
    );
}

#[test]
fn a_ledger_does_not_read_what_disappears_and_its_records_do_not_expire() {
    let src = r#"
ledger Sales:
    who: Text?
    amount: Decimal

entity Sale:
    buyer: Text? personal
    amount: Decimal

    post to Sales:
        add(who=buyer, amount=amount)
"#;
    assert_error(
        src,
        "`Sale.buyer` is personal: `erase` empties it: a ledger keeps its rows for ever",
    );
    let src = r#"
ledger Sales:
    amount: Decimal

entity Sale:
    expire 1 year after created_at
    amount: Decimal

    post to Sales:
        add(amount=amount)
"#;
    assert_error(
        src,
        "Sale posts to Sales: its ledger rows are kept for ever, so its records cannot expire",
    );
}

#[test]
fn a_required_reference_to_records_that_expire() {
    assert_error(
        "entity Session:\n    expire 2 days after created_at\n    token: Text\n\nentity Visit:\n    session: Session\n",
        "`Visit.session` is required, and Session records expire (2 days after created_at): Visit must expire too",
    );
}

#[test]
fn erase_needs_personal_fields_and_a_reason() {
    assert_error(
        "entity C:\n    name: Text\n    action forget:\n        erase self, \"asked\"\n",
        "C has no personal fields: `erase` empties the fields marked `personal`",
    );
    assert_error(
        "entity C:\n    name: Text? personal\n    action forget:\n        erase self\n",
        "`erase` needs its reason",
    );
}
