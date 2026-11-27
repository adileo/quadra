//! Labels shared by the ZPL and PDF tests.

use quadra_label::{Align, Element, Label, Symbology};

pub const SSCC: &str = "37610425002123456";
pub const QR_DATA: &str = "https://quadra.run/t/376104250021234569";

/// A 100 × 150 mm shipping label at 203 dpi: sender, recipient, a QR code, the SSCC as
/// GS1-128, a frame and rules.
pub fn shipping_label() -> Label {
    Label {
        width_mm: 100.0,
        height_mm: 150.0,
        dpi: 203,
        elements: vec![
            Element::Box {
                x_mm: 2.0,
                y_mm: 2.0,
                w_mm: 96.0,
                h_mm: 146.0,
                thickness_mm: 0.5,
            },
            Element::Text {
                x_mm: 5.0,
                y_mm: 5.0,
                size_pt: 9.0,
                bold: false,
                width_mm: Some(90.0),
                align: Align::Left,
                text: "FROM: Quadra Srl, Via Roma 1, 20100 Milano MI, Italia".into(),
            },
            Element::Line {
                x1_mm: 2.0,
                y1_mm: 25.0,
                x2_mm: 98.0,
                y2_mm: 25.0,
                thickness_mm: 0.5,
            },
            Element::Text {
                x_mm: 5.0,
                y_mm: 28.0,
                size_pt: 12.0,
                bold: true,
                width_mm: None,
                align: Align::Left,
                text: "SHIP TO".into(),
            },
            Element::Text {
                x_mm: 5.0,
                y_mm: 34.0,
                size_pt: 14.0,
                bold: false,
                width_mm: Some(60.0),
                align: Align::Left,
                text: "ACME Logistics\nViale Europa 42\n00144 Roma RM".into(),
            },
            Element::Barcode {
                x_mm: 70.0,
                y_mm: 31.0,
                kind: Symbology::QR,
                data: QR_DATA.into(),
                height_mm: 24.0,
                module_mm: None,
                human_readable: false,
            },
            Element::Line {
                x1_mm: 2.0,
                y1_mm: 62.0,
                x2_mm: 98.0,
                y2_mm: 62.0,
                thickness_mm: 0.5,
            },
            Element::Text {
                x_mm: 2.0,
                y_mm: 65.0,
                size_pt: 10.0,
                bold: true,
                width_mm: Some(96.0),
                align: Align::Center,
                text: "SSCC".into(),
            },
            Element::Barcode {
                x_mm: 11.0,
                y_mm: 72.0,
                kind: Symbology::Sscc,
                data: SSCC.into(),
                height_mm: 30.0,
                module_mm: None,
                human_readable: true,
            },
            Element::Line {
                x1_mm: 80.0,
                y1_mm: 145.0,
                x2_mm: 95.0,
                y2_mm: 130.0,
                thickness_mm: 0.3,
            },
            Element::Text {
                x_mm: 5.0,
                y_mm: 140.0,
                size_pt: 8.0,
                bold: false,
                width_mm: Some(70.0),
                align: Align::Right,
                text: "1/1 ^ ~ _".into(),
            },
        ],
    }
}

/// A 2 × 2 pixel PNG: black, white, red, transparent.
pub fn tiny_png() -> Vec<u8> {
    // Written by hand: signature, IHDR (2 × 2, 8-bit RGBA), IDAT (zlib, stored), IEND.
    let raw: Vec<u8> = vec![
        0, 0, 0, 0, 255, 255, 255, 255, 255, //
        0, 255, 0, 0, 255, 0, 0, 0, 0,
    ];
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let chunk = |png: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    let mut z = vec![0x78, 0x01, 0x01];
    z.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    z.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    z.extend_from_slice(&raw);
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for d in data {
        a = (a + *d as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
