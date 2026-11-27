//! `File` fields: what the checker says of the families, the exact types and `max`, and what
//! the model keeps of them for the server.

use quadra_sema::{Project, Severity};
use std::path::{Path, PathBuf};

/// The errors of a project of one file, and its model when there are none.
fn check(qd: &str) -> (Vec<String>, Option<quadra_ir::Model>) {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/__files");
    let project = Project::from_sources(&root, vec![(root.join("model.qd"), qd.to_string())]);
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
    let model = out.is_empty().then_some(model);
    (out, model)
}

fn assert_error(qd: &str, expected: &str) {
    let (e, _) = check(qd);
    assert!(
        e.iter().any(|m| m.contains(expected)),
        "{qd}\nexpected {expected:?}, got {e:#?}"
    );
}

const GOOD: &str = r#"
entity Expense:
    receipt: File<pdf, image>? max 10 MB
    photos: [File<image>] max 2 MB
    contract: File<word>?
    archive: File<"application/zip">?
    anything: File?
    note: Text?
"#;

#[test]
fn file_fields_keep_what_they_take() {
    let (e, m) = check(GOOD);
    assert!(e.is_empty(), "{e:#?}");
    let m = m.unwrap();
    let ent = m.entity("Expense").unwrap();
    let spec = |n: &str| {
        ent.fields
            .iter()
            .find(|f| f.name == n)
            .unwrap()
            .file
            .clone()
    };
    let r = spec("receipt").unwrap();
    assert_eq!(r.accepts_text(), "pdf, image");
    assert_eq!(r.max_bytes(), Some(10 << 20));
    assert!(r.accepts("image/png") && !r.accepts("application/x-msdownload"));
    let p = spec("photos").unwrap();
    assert_eq!(
        (p.accepts_text().as_str(), p.max_bytes()),
        ("image", Some(2 << 20))
    );
    let z = spec("archive").unwrap();
    assert!(z.accepts("application/zip") && !z.accepts("application/pdf"));
    let any = spec("anything").unwrap();
    assert!(any.accept.is_empty() && any.max.is_none());
    assert!(any.accepts("application/x-msdownload"));
    assert!(spec("note").is_none());
}

#[test]
fn an_unknown_family_is_an_error_that_lists_the_families() {
    assert_error(
        "entity E:\n    f: File<pdff>?\n",
        "`pdff` is not a family of files: one of pdf, image, word, excel, text, zip, video, audio",
    );
}

#[test]
fn an_exact_type_is_a_type_of_file() {
    assert_error(
        "entity E:\n    f: File<\"zip\">?\n",
        "`\"zip\"` is not a type of file",
    );
}

#[test]
fn max_is_a_size_of_a_file_field() {
    assert_error(
        "entity E:\n    f: Text max 10 MB\n",
        "`max` is the size of the largest file a `File` field takes: `E.f` is not a `File`",
    );
    assert_error(
        "entity E:\n    f: File? max 10\n",
        "`max` takes a size: `max 10 MB`",
    );
    assert_error("entity E:\n    f: File? max 0 MB\n", "`max` takes a size");
    assert_error("entity E:\n    f: File? max 9 GB\n", "at most 5 GB");
}

#[test]
fn an_attach_goes_to_a_field_that_takes_a_pdf() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/__files_attach");
    let sources = vec![
        (
            root.join("model.qd"),
            "from docs import D\n\nentity Invoice:\n    number: Text\n    pdf: File<image>?\n\nattach D to Invoice.pdf\n".to_string(),
        ),
        (
            root.join("docs.qdp"),
            "from model import Invoice\n\ndocument D(inv: Invoice):\n    text inv.number\n".to_string(),
        ),
    ];
    let project = Project::from_sources(&root, sources);
    let analysis = project.analyze();
    let model = quadra_ir::lower(&project, &analysis);
    let errors: Vec<String> = analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .chain(
            quadra_types::check_model(&model)
                .into_iter()
                .map(|d| d.message),
        )
        .collect();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("`Invoice.pdf` keeps a PDF document, but it takes image")),
        "{errors:#?}"
    );
}
