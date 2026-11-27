//! PDF: the same bytes on every run, and the barcodes read back from the printed page.
//!
//! "Printed" is `pdftoppm` (Poppler) rendering the PDF at 600 dpi, the resolution of a good
//! laser printer; the barcodes are then read from the bitmap by rxing, a port of ZXing.
//! When `pdftoppm` is not installed those checks are skipped (and say so).

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::labels::{shipping_label, tiny_png, QR_DATA};
use common::{crop, decode};
use quadra_label::{to_pdf, Element, Label, Sheet, Symbology};
use rxing::BarcodeFormat;

const DPI: usize = 600;
const GS: char = '\u{1d}';

fn pdftoppm() -> Option<PathBuf> {
    for p in [
        "/opt/homebrew/bin/pdftoppm",
        "/usr/local/bin/pdftoppm",
        "/usr/bin/pdftoppm",
    ] {
        if std::path::Path::new(p).exists() {
            return Some(p.into());
        }
    }
    Command::new("pdftoppm")
        .arg("-v")
        .output()
        .ok()
        .map(|_| PathBuf::from("pdftoppm"))
}

/// Page `page` (1-based) of a PDF as a grayscale bitmap at 600 dpi, or `None` without
/// `pdftoppm`.
fn render(pdf: &[u8], page: usize, name: &str) -> Option<(Vec<u8>, usize, usize)> {
    let Some(tool) = pdftoppm() else {
        eprintln!("pdftoppm is not installed: skipping the check of `{name}` read from print");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("quadra-label-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("in.pdf");
    std::fs::write(&input, pdf).unwrap();
    let prefix = dir.join("page");
    let status = Command::new(tool)
        .args(["-r", &DPI.to_string(), "-gray", "-singlefile"])
        .args(["-f", &page.to_string(), "-l", &page.to_string()])
        .arg(&input)
        .arg(&prefix)
        .status()
        .unwrap();
    assert!(status.success(), "pdftoppm failed");
    let pgm = std::fs::read(dir.join("page.pgm")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    Some(parse_pgm(&pgm))
}

fn parse_pgm(data: &[u8]) -> (Vec<u8>, usize, usize) {
    // "P5", width, height, maxval, each followed by one whitespace character.
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8(data[start..i].to_vec()).unwrap());
    }
    assert_eq!(fields[0], "P5");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let pixels = data[i + 1..i + 1 + w * h].to_vec();
    (pixels, w, h)
}

fn px(mm: f64) -> usize {
    (mm.max(0.0) * DPI as f64 / 25.4).round() as usize
}

/// Reads the barcode of `format` inside the region (mm) of a rendered page.
fn read_region(
    page: &(Vec<u8>, usize, usize),
    region: (f64, f64, f64, f64),
    format: BarcodeFormat,
) -> String {
    let (img, w, h) = page;
    let (x, y, rw, rh) = region;
    let (c, cw, ch) = crop(img, *w, *h, px(x), px(y), px(rw), px(rh));
    decode(c, cw, ch, format)
        .unwrap_or_else(|e| panic!("no {format:?} in {region:?}: {e}"))
        .getText()
        .to_string()
}

#[test]
fn same_bytes_on_every_run() {
    let a = to_pdf(&[shipping_label()], None).unwrap();
    let b = to_pdf(&[shipping_label()], None).unwrap();
    assert_eq!(a, b);
    assert!(a.starts_with(b"%PDF-"));
    let text = String::from_utf8_lossy(&a);
    assert!(text.contains("/ID ["));
    assert!(!text.contains("CreationDate"));
    assert!(text.contains("/BaseFont /Helvetica"));
    // A different label gives a different document identifier.
    let mut other = shipping_label();
    other.elements.pop();
    let c = to_pdf(&[other], None).unwrap();
    let id = |pdf: &[u8]| {
        let t = String::from_utf8_lossy(pdf).to_string();
        let k = t.find("/ID [").unwrap();
        t[k..k + 80].to_string()
    };
    assert_ne!(id(&a), id(&c));
}

#[test]
fn shipping_label_read_from_print() {
    let pdf = to_pdf(&[shipping_label()], None).unwrap();
    let Some(page) = render(&pdf, 1, "shipping") else {
        return;
    };
    // 100 × 150 mm at 600 dpi.
    assert!(page.1.abs_diff(2362) <= 1 && page.2.abs_diff(3543) <= 1);
    // The SSCC as GS1-128: bars from x = 11 mm, 30 mm tall at y = 72 mm, 5 mm quiet zones.
    assert_eq!(
        read_region(&page, (5.0, 70.0, 90.0, 34.0), BarcodeFormat::CODE_128),
        "]C100376104250021234569"
    );
    // The QR: 24 mm at (70, 31), quiet zone included in the region.
    assert_eq!(
        read_region(&page, (66.0, 27.0, 33.0, 32.0), BarcodeFormat::QR_CODE),
        QR_DATA
    );
}

fn small(kind: Symbology, data: &str, x: f64, y: f64, h: f64) -> Element {
    Element::Barcode {
        x_mm: x,
        y_mm: y,
        kind,
        data: data.into(),
        height_mm: h,
        module_mm: None,
        human_readable: true,
    }
}

#[test]
fn sheet_of_labels_read_from_print() {
    // An A4 sheet of 3 × 8 labels of 70 × 37 mm; 26 labels make two pages.
    let sheet = Sheet {
        page_w_mm: 210.0,
        page_h_mm: 297.0,
        cols: 3,
        rows: 8,
        margin_left_mm: 0.0,
        margin_top_mm: 0.5,
        gap_x_mm: 0.0,
        gap_y_mm: 0.0,
    };
    let make = |i: usize| Label {
        width_mm: 70.0,
        height_mm: 37.0,
        dpi: 203,
        elements: match i % 3 {
            0 => vec![small(
                Symbology::Ean13,
                &format!("40063813339{}", i % 10)
                    .chars()
                    .take(12)
                    .collect::<String>(),
                19.0,
                4.0,
                20.0,
            )],
            1 => vec![
                small(
                    Symbology::DataMatrix,
                    &format!("(01)09501101530003(10)LOT{i}(21)S{i}"),
                    5.0,
                    5.0,
                    20.0,
                ),
                Element::Image {
                    x_mm: 40.0,
                    y_mm: 5.0,
                    w_mm: 20.0,
                    h_mm: 20.0,
                    png: tiny_png(),
                },
            ],
            _ => vec![small(
                Symbology::Code128,
                &format!("ITEM-{i:04}"),
                10.0,
                4.0,
                20.0,
            )],
        },
    };
    let labels: Vec<Label> = (0..26).map(make).collect();
    let pdf = to_pdf(&labels, Some(&sheet)).unwrap();
    assert_eq!(pdf, to_pdf(&labels, Some(&sheet)).unwrap());
    assert!(String::from_utf8_lossy(&pdf).contains("/Count 2"));
    let Some(page) = render(&pdf, 1, "sheet") else {
        return;
    };
    assert!(page.1.abs_diff(4961) <= 1 && page.2.abs_diff(7016) <= 1);
    let mut read = 0;
    for i in 0..24 {
        let (col, row) = ((i % 3) as f64, (i / 3) as f64);
        let (ox, oy) = (col * 70.0, 0.5 + row * 37.0);
        let region = (ox, oy, 70.0, 37.0);
        let ean = |i: usize| {
            let body: String = format!("40063813339{}", i % 10).chars().take(12).collect();
            format!("{body}{}", quadra_label::check_digit(&body).unwrap())
        };
        match i % 3 {
            0 => assert_eq!(read_region(&page, region, BarcodeFormat::EAN_13), ean(i)),
            1 => assert_eq!(
                read_region(&page, (ox, oy, 32.0, 32.0), BarcodeFormat::DATA_MATRIX),
                format!("0109501101530003 10LOT{i}{GS}21S{i}").replace(' ', "")
            ),
            _ => assert_eq!(
                read_region(&page, region, BarcodeFormat::CODE_128),
                format!("ITEM-{i:04}")
            ),
        }
        read += 1;
    }
    assert_eq!(read, 24);
    // The second page has the last two labels.
    let Some(page2) = render(&pdf, 2, "sheet2") else {
        return;
    };
    assert_eq!(
        read_region(&page2, (0.0, 0.5, 70.0, 37.0), BarcodeFormat::EAN_13),
        "4006381333948"
    );
    assert_eq!(
        read_region(&page2, (70.0, 0.5, 32.0, 32.0), BarcodeFormat::DATA_MATRIX),
        format!("0109501101530003 10LOT25{GS}21S25").replace(' ', "")
    );
}

#[test]
fn sheet_validation() {
    let sheet = Sheet {
        page_w_mm: 210.0,
        page_h_mm: 297.0,
        cols: 3,
        rows: 8,
        margin_left_mm: 5.0,
        margin_top_mm: 0.5,
        gap_x_mm: 0.0,
        gap_y_mm: 0.0,
    };
    let l = |w: f64| Label {
        width_mm: w,
        height_mm: 37.0,
        dpi: 203,
        elements: vec![],
    };
    assert_eq!(
        to_pdf(&[l(70.0)], Some(&sheet)).unwrap_err(),
        "3 columns of 70 mm labels end at 215 mm, past the page width of 210 mm"
    );
    assert_eq!(
        to_pdf(&[l(60.0), l(65.0)], Some(&sheet)).unwrap_err(),
        "label 2 is 65 × 37 mm but the labels on the sheet are 60 × 37 mm (label 1): all \
labels on a sheet have the same size"
    );
    assert_eq!(to_pdf(&[], None).unwrap_err(), "there are no labels");
}
