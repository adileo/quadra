use quadra_print::*;

fn invoice() -> Doc {
    let mut lines = Vec::new();
    for i in 1..=60 {
        lines.push(vec![
            format!("House blend #{i} \"special\" #[x]"),
            "2 kg".into(),
            "36.00 EUR".into(),
        ]);
    }
    Doc {
        title: Some("Invoice INV/2026/00001".into()),
        header: vec![Block::Row {
            widths: vec![2.0, 1.0],
            gap: 4.0,
            valign: "top".into(),
            cells: vec![
                vec![Block::Text {
                    text: "Beans & Leaves".into(),
                    style: Style {
                        bold: true,
                        size: Some(14.0),
                        ..Style::default()
                    },
                }],
                vec![Block::Text {
                    text: "Via Roma 1\nMilano".into(),
                    style: Style {
                        align: Align::Right,
                        ..Style::default()
                    },
                }],
            ],
        }],
        footer: vec![Block::Pages {
            format: "Page {page} of {pages}".into(),
            style: Style {
                align: Align::Center,
                size: Some(8.0),
                ..Style::default()
            },
        }],
        body: vec![
            Block::Heading {
                text: "Invoice INV/2026/00001".into(),
                level: 1,
                style: Style::default(),
            },
            Block::Text {
                text: "Caffè Centrale".into(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            },
            Block::Space(6.0),
            Block::Table {
                columns: vec![
                    Column {
                        title: "Product".into(),
                        width: 3.0,
                        align: Align::Left,
                        bold: false,
                    },
                    Column {
                        title: "Qty".into(),
                        width: 1.0,
                        align: Align::Right,
                        bold: false,
                    },
                    Column {
                        title: "Amount".into(),
                        width: 1.0,
                        align: Align::Right,
                        bold: false,
                    },
                ],
                rows: lines,
                header: true,
                size: None,
            },
            Block::Totals {
                items: vec![
                    ("Subtotal".into(), "162.00 EUR".into(), false),
                    ("Total".into(), "187.92 EUR".into(), true),
                ],
                width: None,
                size: None,
            },
            Block::Rule {
                thickness: 0.5,
                color: Some("#1f4e79".into()),
            },
            Block::Typst("#text(8pt)[Thank you.]".into()),
        ],
        ..Doc::default()
    }
}

#[test]
fn an_invoice_renders_on_pages_with_their_numbers() {
    let d = invoice();
    let pdf =
        render(&d, &[], "Invoice#45").unwrap_or_else(|e| panic!("{e}\n{}", doc::to_typst(&d)));
    assert_eq!(pdf.pages, 2, "{}", pdf.text);
    assert!(pdf.text.contains("Page 2 of 2"), "{}", pdf.text);
    // the data is text, never markup: quotes, `#` and brackets come out as written
    assert!(
        pdf.text.contains("House blend #7 \"special\" #[x]"),
        "{}",
        pdf.text
    );
    assert_eq!(render(&d, &[], "Invoice#45").unwrap().bytes, pdf.bytes);
}
