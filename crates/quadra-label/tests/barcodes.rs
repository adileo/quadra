//! Every symbology encoded, drawn and read back by a decoder; check digits; error messages.

mod common;

use common::{decode, rasterize, symbology_id};
use quadra_label::{check_digit, encode, svg, Modules, QrEcc, SvgOptions, Symbology};
use rxing::BarcodeFormat;

const GS: char = '\u{1d}';

fn read(kind: Symbology, data: &str, format: BarcodeFormat) -> rxing::RXingResult {
    let modules = encode(kind, data).unwrap_or_else(|e| panic!("{data}: {e}"));
    let (img, w, h) = rasterize(&modules, kind.quiet_zone() as usize);
    decode(img, w, h, format).unwrap_or_else(|e| panic!("{} `{data}` not read: {e}", kind.name()))
}

#[test]
fn code128_round_trips() {
    for data in [
        "Hello, World!",
        "ABC-12345678",
        "x",
        "0",
        "12",
        "123456789012",
        "a1b22c333d4444e55555",
        "LOT 2026/11 >< ~^_",
        "tab\there",
        "\u{7f}del",
    ] {
        let r = read(Symbology::Code128, data, BarcodeFormat::CODE_128);
        assert_eq!(r.getText(), data);
    }
}

#[test]
fn code128_uses_code_set_c_for_digits() {
    // 12 digits in code set C: start, 6 pairs and check are 8 × 11 modules, the stop 13.
    let Modules::Linear { bars, .. } = encode(Symbology::Code128, "123456789012").unwrap() else {
        panic!()
    };
    assert_eq!(bars.len(), 8 * 11 + 13);
}

#[test]
fn ean13_round_trips() {
    let r = read(Symbology::Ean13, "400638133393", BarcodeFormat::EAN_13);
    assert_eq!(r.getText(), "4006381333931");
    let r = read(Symbology::Ean13, "9780201379624", BarcodeFormat::EAN_13);
    assert_eq!(r.getText(), "9780201379624");
    for first in 0..10 {
        let body = format!("{first}12345678901");
        let full = format!("{body}{}", check_digit(&body).unwrap());
        assert_eq!(
            read(Symbology::Ean13, &body, BarcodeFormat::EAN_13).getText(),
            full
        );
    }
}

#[test]
fn gs1_128_round_trips_with_fnc1() {
    // FNC1 first gives the `]C1` prefix; (10) is last, so no separator.
    let r = read(
        Symbology::Gs1_128,
        "(01)09501101530003(17)261231(10)LOT42",
        BarcodeFormat::CODE_128,
    );
    assert_eq!(
        r.getText(),
        "]C1010950110153000317261231 10LOT42".replace(' ', "")
    );
    // (10) is variable length and not last: FNC1 after it, read back as GS.
    let r = read(
        Symbology::Gs1_128,
        "(10)LOT42(17)261231(21)S/N-7",
        BarcodeFormat::CODE_128,
    );
    assert_eq!(r.getText(), format!("]C110LOT42{GS}17261231") + "21S/N-7");
    let r = read(
        Symbology::Gs1_128,
        "(400)PO-7781(3103)001250(37)12(421)380123",
        BarcodeFormat::CODE_128,
    );
    assert_eq!(
        r.getText(),
        format!("]C1400PO-7781{GS}310300125037 12{GS}421380123").replace(' ', "")
    );
    let Modules::Linear { text, .. } =
        encode(Symbology::Gs1_128, "(01)09501101530003(17)261231(10)LOT42").unwrap()
    else {
        panic!()
    };
    assert_eq!(text, "(01)09501101530003(17)261231(10)LOT42");
}

#[test]
fn sscc_round_trips() {
    let r = read(
        Symbology::Sscc,
        "37610425002123456",
        BarcodeFormat::CODE_128,
    );
    assert_eq!(r.getText(), "]C100376104250021234569");
    let r = read(
        Symbology::Sscc,
        "006141411234567890",
        BarcodeFormat::CODE_128,
    );
    assert_eq!(r.getText(), "]C100006141411234567890");
    let Modules::Linear { bars, text } = encode(Symbology::Sscc, "00106141411234567").unwrap()
    else {
        panic!()
    };
    assert_eq!(text, "(00)001061414112345672");
    // Start C, FNC1, 10 pairs and check (11 modules each), stop (13).
    assert_eq!(bars.len(), 13 * 11 + 13);
}

#[test]
fn qr_round_trips() {
    for ecc in [QrEcc::L, QrEcc::M, QrEcc::Q, QrEcc::H] {
        for data in [
            "0123456789012345678901234567890",
            "HELLO WORLD $%*+-./:",
            "https://quadra.run/l/ABC123?x=1",
            "Grüße, café, 日本語, ☕",
            "MIXED 12345678901234567890 text",
        ] {
            let r = read(Symbology::Qr(ecc), data, BarcodeFormat::QR_CODE);
            assert_eq!(r.getText(), data, "{ecc:?}");
        }
    }
}

#[test]
fn qr_chooses_numeric_mode() {
    // 41 digits fit version 1 (21 × 21) at L only in numeric mode.
    let Modules::Matrix { size_w, .. } = encode(
        Symbology::Qr(QrEcc::L),
        "12345678901234567890123456789012345678901",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(size_w, 21);
    // 25 alphanumeric characters fit version 1 at L in alphanumeric mode.
    let Modules::Matrix { size_w, .. } =
        encode(Symbology::Qr(QrEcc::L), "HELLO WORLD 1234567890ABC").unwrap()
    else {
        panic!()
    };
    assert_eq!(size_w, 21);
}

#[test]
fn datamatrix_round_trips() {
    let long_text: String = (0..600).map(|i| (b'A' + (i % 26) as u8) as char).collect();
    let longest: String = "A".repeat(1400);
    for data in [
        "Hello",
        "1234567890",
        "a",
        "Grüße aus Köln",
        "日本語のテキスト",
        long_text.as_str(),
        longest.as_str(),
    ] {
        let r = read(Symbology::DataMatrix, data, BarcodeFormat::DATA_MATRIX);
        assert_eq!(r.getText(), data);
    }
}

#[test]
fn datamatrix_sizes() {
    let side = |data: &str| encode(Symbology::DataMatrix, data).unwrap().width();
    assert_eq!(side("A"), 10);
    assert_eq!(side("123456"), 10);
    assert_eq!(side("1234567"), 12);
    assert_eq!(side(&"A".repeat(1400)), 144);
    assert_eq!(side(&"7".repeat(3116)), 144);
    assert!(encode(Symbology::DataMatrix, &"A".repeat(1559))
        .unwrap_err()
        .contains("too long"));
}

#[test]
fn gs1_datamatrix_round_trips() {
    let r = read(
        Symbology::DataMatrix,
        "(01)09501101530003(17)261231(10)LOT42(21)ABC",
        BarcodeFormat::DATA_MATRIX,
    );
    assert_eq!(
        r.getText(),
        format!("010950110153000317261231 10LOT42{GS}21ABC").replace(' ', "")
    );
    assert_eq!(symbology_id(&r).as_deref(), Some("]d2"));
}

#[test]
fn check_digits() {
    assert_eq!(check_digit("400638133393"), Ok(1));
    // GS1 General Specifications, the check digit calculation example (SSCC).
    assert_eq!(check_digit("37610425002123456"), Ok(9));
    // The SSCC of the GS1 logistic label examples: (00) 0 0614141 123456789 0.
    assert_eq!(check_digit("00614141123456789"), Ok(0));
    assert_eq!(check_digit("00106141411234567"), Ok(2));
    // GTIN-14 of the GS1 examples.
    assert_eq!(check_digit("0950110153000"), Ok(3));
    let Modules::Linear { text, .. } = encode(Symbology::Ean13, "400638133393").unwrap() else {
        panic!()
    };
    assert_eq!(text, "4006381333931");
}

#[test]
fn errors_say_what_and_where() {
    let err = |kind, data| encode(kind, data).unwrap_err();
    assert_eq!(
        err(Symbology::Ean13, "12345678901"),
        "EAN-13 takes 12 or 13 digits, not 11"
    );
    assert_eq!(
        err(Symbology::Ean13, "4006381333932"),
        "the check digit of `4006381333932` should be 1"
    );
    assert_eq!(
        err(Symbology::Ean13, "40063813339x"),
        "EAN-13 takes digits only; `x` at position 12 is not a digit"
    );
    assert_eq!(
        err(Symbology::Sscc, "3761042500212345"),
        "SSCC takes 17 digits (extension digit, company prefix and serial reference) or 18 \
with the check digit, not 16"
    );
    assert_eq!(
        err(Symbology::Sscc, "376104250021234560"),
        "the check digit of SSCC `376104250021234560` should be 9"
    );
    assert_eq!(
        err(Symbology::Gs1_128, "(01)09501101530004"),
        "GS1: the check digit of (01) `09501101530004` should be 3"
    );
    assert_eq!(
        err(Symbology::Gs1_128, "(01)0950110153000"),
        "GS1: the value of (01) (GTIN) must be 14 digits, not 13: `0950110153000`"
    );
    assert_eq!(
        err(Symbology::Gs1_128, "(17)261331"),
        "GS1: (17) is a date YYMMDD; `261331` has month 13"
    );
    assert_eq!(
        err(Symbology::Gs1_128, "(17)260230"),
        "GS1: (17) is a date YYMMDD; `260230` has day 30, and month 02 has 29 days"
    );
    assert_eq!(
        err(Symbology::Gs1_128, "(10)ABCDEFGHIJKLMNOPQRSTU"),
        "GS1: (10) (batch or lot number) holds at most 20 characters, not 21: \
`ABCDEFGHIJKLMNOPQRSTU`"
    );
    assert!(err(Symbology::Gs1_128, "(99)ABC")
        .starts_with("GS1: application identifier (99) is not supported (supported: 00, 01,"));
    assert!(err(Symbology::Gs1_128, "01095011015300")
        .starts_with("GS1 data starts with an application identifier in parentheses"));
    assert!(err(Symbology::Gs1_128, "(10)LOTé")
        .starts_with("GS1: `é` is not allowed in the value of (10)"));
    assert_eq!(
        err(Symbology::Gs1_128, "(10)A(10)B"),
        "GS1: application identifier (10) appears twice"
    );
    assert_eq!(
        err(Symbology::Code128, "Café"),
        "Code 128 holds ASCII characters only; `é` at position 4 is not"
    );
    assert!(err(
        Symbology::Gs1_128,
        "(400)ABCDEFGHIJKLMNOPQRSTUVWXYZ0123(401)ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"
    )
    .starts_with("GS1-128 holds at most 48 characters"));
}

#[test]
fn names() {
    assert_eq!(Symbology::from_name("GS1-128"), Some(Symbology::Gs1_128));
    assert_eq!(
        Symbology::from_name("Data Matrix"),
        Some(Symbology::DataMatrix)
    );
    assert_eq!(Symbology::from_name("qr"), Some(Symbology::Qr(QrEcc::M)));
    assert_eq!(Symbology::from_name("QR-H"), Some(Symbology::Qr(QrEcc::H)));
    assert_eq!(Symbology::from_name("ean13"), Some(Symbology::Ean13));
    assert_eq!(Symbology::from_name("pdf417"), None);
}

#[test]
fn svg_is_minimal_and_stable() {
    let opts = SvgOptions::default();
    let a = svg(Symbology::Ean13, "400638133393", &opts).unwrap();
    let b = svg(Symbology::Ean13, "400638133393", &opts).unwrap();
    assert_eq!(a, b);
    // EAN-13 has 30 bars; 95 modules + 2 × 11 quiet = 117 modules of 0.33 mm.
    assert_eq!(a.matches("<rect x=").count(), 30);
    assert!(a.starts_with(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"38.610mm\" height=\"18.322mm\" \
viewBox=\"0 0 38.610 18.322\""
    ));
    assert!(a.contains(">4006381333931</text>"));
    let q = svg(
        Symbology::QR,
        "HELLO",
        &SvgOptions {
            module_mm: 1.0,
            human_readable: false,
            ..opts
        },
    )
    .unwrap();
    // Version 1 with a 4-module quiet zone: 29 mm.
    assert!(q.contains("viewBox=\"0 0 29.000 29.000\""));
    assert!(!q.contains("<text"));
    // Runs are merged: no two rects of the same row touch.
    let modules = encode(Symbology::QR, "HELLO").unwrap();
    let Modules::Matrix { dark, size_w, .. } = modules else {
        panic!()
    };
    let runs: usize = dark
        .chunks(size_w)
        .map(|row| {
            row.iter()
                .zip(row.iter().skip(1))
                .filter(|(a, b)| **a && !**b)
                .count()
                + usize::from(row[size_w - 1])
        })
        .sum();
    assert_eq!(q.matches("<rect x=").count(), runs);
}
