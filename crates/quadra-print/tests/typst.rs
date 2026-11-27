use quadra_print::typst::pdf;

const SRC: &str = r#"
#set document(title: "Invoice INV/2026/00001", date: none)
#set page(paper: "a4", margin: 20mm, footer: context align(center, text(8pt)[Page #counter(page).display() of #counter(page).final().first()]))
#set text(font: "Libertinus Serif", size: 10pt, lang: "en")
= Invoice INV/2026/00001
#table(columns: (3fr, 1fr, 1fr), table.header[*Product*][*Qty*][*Amount*],
  ..range(1, 80).map(i => ([House blend #i], [2 kg], [36.00 EUR])).flatten())
"#;

#[test]
fn the_same_source_gives_the_same_bytes() {
    let a = pdf(SRC, &[], "Invoice#45").unwrap();
    let b = pdf(SRC, &[], "Invoice#45").unwrap();
    assert_eq!(a.bytes, b.bytes);
    assert!(a.bytes.starts_with(b"%PDF-"));
    assert_eq!(a.pages, 2, "{}", a.text);
    assert!(a.text.contains("Invoice INV/2026/00001"));
    assert!(a.text.contains("Page"));
    // another ident: another /ID, nothing else
    let c = pdf(SRC, &[], "Invoice#46").unwrap();
    assert_ne!(a.bytes, c.bytes);
    assert_eq!(a.bytes.len(), c.bytes.len());
}

#[test]
fn an_error_says_what() {
    let e = pdf("#nope()", &[], "x").unwrap_err();
    assert!(e.contains("unknown variable: nope"), "{e}");
}

#[test]
fn pdftotext_reads_it() {
    let Ok(_) = std::process::Command::new("pdftotext").arg("-v").output() else {
        eprintln!("pdftotext is not installed: skipped");
        return;
    };
    let a = pdf(SRC, &[], "Invoice#45").unwrap();
    let dir = std::env::temp_dir().join(format!("quadra-print-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.pdf");
    std::fs::write(&f, &a.bytes).unwrap();
    let out = std::process::Command::new("pdftotext")
        .arg(&f)
        .arg("-")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("House blend 79"), "{text}");
    assert!(text.contains("Page 2 of 2"), "{text}");
}
