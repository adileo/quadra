//! Formulas of a frozen document that gather records coming after it: said, with the fix.

use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

fn warnings(edit: impl Fn(&str, String) -> String) -> Vec<String> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/extended");
    let base = Project::load(&root).unwrap();
    let sources = base
        .user_files()
        .map(|f| {
            (
                f.path.clone(),
                edit(
                    &f.path.file_name().unwrap().to_string_lossy(),
                    f.text.clone(),
                ),
            )
        })
        .collect();
    let checked = quadra_analysis::check(&Project::from_sources(&root, sources));
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn the_example_declares_what_follows_the_data() {
    let w = warnings(|_, t| t);
    assert!(!w.iter().any(|m| m.contains("live")), "{w:#?}");
}

#[test]
fn without_live_the_invoices_of_a_confirmed_order_are_flagged() {
    let w = warnings(|name, t| {
        if name == "invoicing.qd" {
            t.replace("    live invoices, invoice_status\n", "")
        } else {
            t
        }
    });
    assert!(
        w.iter().any(
            |m| m.starts_with("`invoices` reads InvoiceLine (via origin)")
                && m.contains("declare `live invoices` in this `extend`")
        ),
        "{w:#?}"
    );
    assert!(
        w.iter().any(|m| m.starts_with("`invoice_status`")),
        "{w:#?}"
    );
    // frozen only when done: the snapshot is right, nothing said
    assert!(!w.iter().any(|m| m.contains("ProductionOrder")), "{w:#?}");
}

#[test]
fn the_lines_of_a_frozen_document_are_frozen_with_it() {
    let w = warnings(|name, t| {
        if name == "sales.qd" {
            t.replace("    live delivered, to_deliver\n", "")
        } else {
            t
        }
    });
    assert!(
        w.iter().any(|m| m.starts_with("`delivered` reads StockMove (via sale_line): records that point to the SalesLine and come after its SalesOrder is frozen. On a line of a frozen SalesOrder")
            && m.ends_with("declare `live delivered`")),
        "{w:#?}"
    );
    assert!(w.iter().any(|m| m.starts_with("`to_deliver`")), "{w:#?}");
}

/// 34. A warning about a declaration covers its first line only, not the blank lines and the
/// declarations that follow.
#[test]
fn warnings_cover_the_declaration_line() {
    let src = "entity Order:\n    state: enum(Draft, Confirmed, Done, Archived) default Draft\n    flow state:\n        Draft -> Confirmed by confirm\n        Confirmed -> Done by finish\n    frozen when state is Confirmed\n    invoiced = Invoice.where(i => i.order == self).sum(i => i.amount)\n\n\n    other: Int default 0\n\nentity Invoice:\n    order: Order\n    amount: Int\n\n\nmetric total() -> Int:\n    label: \"x\"\n    total = Invoice.all().sum(i => i.amount)\n\n\nfn f() -> Int:\n    return 1\n";
    let dir = std::env::temp_dir().join(format!("quadra-spans-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("metrics.lock"), "").unwrap();
    let project = Project::from_sources(&dir, vec![(dir.join("main.qd"), src.to_string())]);
    let checked = quadra_analysis::check(&project);
    let warnings: Vec<_> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .collect();
    assert!(warnings.len() >= 3, "{warnings:#?}");
    for w in warnings {
        let text = &src[usize::from(w.range.start())..usize::from(w.range.end())];
        assert!(
            !text.contains('\n') && !text.is_empty(),
            "{}: {text:?}",
            w.message
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
