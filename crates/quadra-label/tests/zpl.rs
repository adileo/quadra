//! ZPL: the exact output for a shipping label, the conversion to dots, validation.

mod common;

use common::labels::{shipping_label, tiny_png};
use quadra_label::{to_zpl, Align, Element, Label, Symbology};

/// The shipping label of `common::labels`, 100 × 150 mm at 203 dpi.
///
/// Dots are `round(mm × dpi / 25.4)` with halves away from zero: 100 mm is 799.2 dots so
/// `^PW799`, 150 mm is 1198.8 so `^LL1199`. The SSCC is GS1-128 in code set C after FNC1
/// (`>;>8`), 0.5 mm modules are 4 dots, the bars 30 mm = 240 dots tall.
const GOLDEN: &str = "\
^XA
^CI28
^PW799
^LL1199
^LH0,0
^FO16,16^GB767,1167,4,B,0^FS
^FO40,40^A0N,25,25^FB719,1,4,L,0^FDFROM: Quadra Srl, Via Roma 1, 20100 Milano MI, Italia^FS
^FO16,198^GB767,4,4,B,0^FS
^FO40,224^A0N,34,34^FB759,1,5,L,0^FDSHIP TO^FS
^FO40,272^A0N,39,39^FB479,3,6,L,0^FDACME Logistics\\&Viale Europa 42\\&00144 Roma RM^FS
^FO559,248^BQN,2,6,M^FDMA,https://quadra.run/t/376104250021234569^FS
^FO16,494^GB767,4,4,B,0^FS
^FO16,519^A0N,28,28^FB767,1,4,C,0^FDSSCC^FS
^FO88,575^BY4^BCN,240,N,N,N,N^FD>;>800376104250021234569^FS
^FO88,819^A0N,23,23^FB623,1,3,C,0^FD(00)376104250021234569^FS
^FO639,1039^GD120,120,2,B,R^FS
^FO40,1119^A0N,23,23^FB559,1,3,R,0^FH^FD1/1 _5E _7E _5F^FS
^XZ
";

#[test]
fn shipping_label_golden() {
    let zpl = to_zpl(&[shipping_label()]).unwrap();
    assert_eq!(zpl, GOLDEN);
    // Same input, same bytes.
    assert_eq!(to_zpl(&[shipping_label()]).unwrap(), zpl);
}

fn label(dpi: u32, elements: Vec<Element>) -> Label {
    Label {
        width_mm: 100.0,
        height_mm: 150.0,
        dpi,
        elements,
    }
}

fn barcode(kind: Symbology, data: &str, x: f64, y: f64) -> Element {
    Element::Barcode {
        x_mm: x,
        y_mm: y,
        kind,
        data: data.into(),
        height_mm: 20.0,
        module_mm: None,
        human_readable: false,
    }
}

#[test]
fn dots_at_every_resolution() {
    for (dpi, pw, ll) in [
        (152, 598, 898),
        (203, 799, 1199),
        (300, 1181, 1772),
        (600, 2362, 3543),
    ] {
        let zpl = to_zpl(&[label(dpi, vec![])]).unwrap();
        assert!(zpl.contains(&format!("^PW{pw}\n^LL{ll}\n")), "{dpi}: {zpl}");
    }
    // 0.5 mm modules: 3 dots at 152 dpi, 4 at 203, 6 at 300, 10 (the ^BY maximum) at 600.
    for (dpi, m) in [(152, 3), (203, 4), (300, 6), (600, 10)] {
        let zpl = to_zpl(&[label(
            dpi,
            vec![barcode(Symbology::Sscc, "37610425002123456", 6.0, 10.0)],
        )])
        .unwrap();
        assert!(zpl.contains(&format!("^BY{m}^BC")), "{dpi}: {zpl}");
    }
}

#[test]
fn several_labels_and_symbologies() {
    let elements = vec![
        barcode(Symbology::Ean13, "400638133393", 10.0, 5.0),
        barcode(Symbology::Code128, "A>b\tc", 10.0, 30.0),
        barcode(Symbology::Code128, "123456ab", 10.0, 55.0),
        barcode(
            Symbology::DataMatrix,
            "(01)09501101530003(17)261231(10)LOT42(21)ABC",
            10.0,
            80.0,
        ),
        barcode(Symbology::DataMatrix, "tilde~caret^under_", 50.0, 80.0),
        barcode(Symbology::Qr(quadra_label::QrEcc::H), "HELLO", 50.0, 110.0),
        Element::Image {
            x_mm: 10.0,
            y_mm: 110.0,
            w_mm: 2.0,
            h_mm: 2.0,
            png: tiny_png(),
        },
        Element::Line {
            x1_mm: 90.0,
            y1_mm: 110.0,
            x2_mm: 90.0,
            y2_mm: 140.0,
            thickness_mm: 1.0,
        },
        Element::Line {
            x1_mm: 10.0,
            y1_mm: 120.0,
            x2_mm: 30.0,
            y2_mm: 140.0,
            thickness_mm: 0.25,
        },
    ];
    let zpl = to_zpl(&[label(203, elements.clone()), label(300, elements)]).unwrap();
    assert_eq!(zpl.matches("^XA").count(), 2);
    assert_eq!(zpl.matches("^XZ\n").count(), 2);
    let first = zpl.split("^XZ\n").next().unwrap();
    let lines: Vec<&str> = first.lines().collect();
    // EAN-13: 0.33 mm is 3 dots at 203 dpi; 12 digits, the printer adds the check digit.
    assert_eq!(lines[5], "^FO80,40^BY3^BEN,160,N,N^FD400638133393^FS");
    // `>` is `><`, the tab needs code set A and `^FH`.
    assert_eq!(
        lines[6],
        "^FO80,240^BY3^BCN,160,N,N,N,N^FH^FD>:A><b>7_09>6c^FS"
    );
    // Digits first in code set C, then code set B.
    assert_eq!(lines[7], "^FO80,440^BY3^BCN,159,N,N,N,N^FD>;123456>6ab^FS");
    // GS1 Data Matrix: `_1` is FNC1, first and after (10).
    assert_eq!(
        lines[8],
        "^FO80,639^BXN,7,200,22,22,,_,1^FD_10109501101530003172612311 0LOT42_121ABC^FS"
            .replace(' ', "")
    );
    // `_` is in the data, so `|` is the escape character and `!` the ^FH indicator.
    assert_eq!(
        lines[9],
        "^FO400,639^BXN,8,200,18,18,,|,1^FH!^FDtilde!7Ecaret!5Eunder_^FS"
    );
    assert_eq!(lines[10], "^FO400,879^BQN,2,7,H^FDHA,HELLO^FS");
    // 2 mm at 203 dpi is 16 dots: 2 bytes per row, 16 rows.
    assert!(lines[11].starts_with("^FO80,879^GFA,32,32,2,FF00FF00"));
    assert_eq!(lines[12], "^FO715,879^GB8,240,8,B,0^FS");
    assert_eq!(lines[13], "^FO80,959^GD160,160,2,B,L^FS");
}

#[test]
fn image_bits() {
    let zpl = to_zpl(&[label(
        203,
        vec![Element::Image {
            x_mm: 10.0,
            y_mm: 10.0,
            w_mm: 2.0,
            h_mm: 2.0,
            png: tiny_png(),
        }],
    )])
    .unwrap();
    let line = zpl.lines().find(|l| l.contains("^GFA")).unwrap();
    let hex = line.split(',').nth(5).unwrap().trim_end_matches("^FS");
    // Rows 0-7: left half black (the black pixel), right half white; rows 8-15: left half
    // black (the red pixel is dark), right half white (transparent over white).
    assert_eq!(hex, "FF00".repeat(16));
}

#[test]
fn validation_names_the_element() {
    let err = |l: Label| to_zpl(&[l]).unwrap_err();
    assert_eq!(
        err(label(250, vec![])),
        "label 1: the resolution is 250 dpi; it must be 152, 203, 300 or 600"
    );
    assert_eq!(
        err(label(
            203,
            vec![Element::Box {
                x_mm: 50.0,
                y_mm: 10.0,
                w_mm: 60.0,
                h_mm: 10.0,
                thickness_mm: 0.5
            }]
        )),
        "label 1, element 1 (box): the box ends at x = 110 mm, past the right edge of the label (100 mm)"
    );
    assert_eq!(
        err(label(
            203,
            vec![barcode(Symbology::Sscc, "37610425002123456", 2.0, 10.0)]
        )),
        "label 1, element 1 (SSCC `37610425002123456`): the barcode with its quiet zone (5 mm) \
starts 3 mm left of the label"
    );
    assert_eq!(
        err(label(203, vec![barcode(Symbology::Ean13, "4006381333932", 10.0, 10.0)])),
        "label 1, element 1 (EAN-13 `4006381333932`): the check digit of `4006381333932` should be 1"
    );
    assert_eq!(
        err(label(
            203,
            vec![Element::Text {
                x_mm: 5.0,
                y_mm: 146.0,
                size_pt: 12.0,
                bold: false,
                width_mm: None,
                align: Align::Left,
                text: "Bottom".into()
            }]
        )),
        "label 1, element 1 (text `Bottom`): the text (1 lines) ends at y = 150.87 mm, past the \
bottom of the label (150 mm)"
    );
    assert_eq!(
        err(label(
            203,
            vec![Element::Text {
                x_mm: 5.0,
                y_mm: 5.0,
                size_pt: 12.0,
                bold: false,
                width_mm: Some(30.0),
                align: Align::Left,
                text: "中文".into()
            }]
        )),
        "label 1, element 1 (text `中文`): the label fonts cannot print `中` (U+4E2D): text is \
limited to the Windows-1252 characters (Western European languages)"
    );
    assert_eq!(
        err(label(
            203,
            vec![Element::Line {
                x1_mm: 5.0,
                y1_mm: 5.0,
                x2_mm: 5.0,
                y2_mm: 5.0,
                thickness_mm: 1.0
            }]
        )),
        "label 1, element 1 (line): the line has no length: both ends are the same point"
    );
    assert_eq!(
        err(label(
            203,
            vec![Element::Image {
                x_mm: 5.0,
                y_mm: 5.0,
                w_mm: 5.0,
                h_mm: 5.0,
                png: b"not a png".to_vec()
            }]
        ))
        .split(':')
        .take(2)
        .collect::<Vec<_>>()
        .join(":"),
        "label 1, element 1 (image): the image is not a PNG that can be read"
    );
    assert!(to_zpl(&[]).is_err());
}
