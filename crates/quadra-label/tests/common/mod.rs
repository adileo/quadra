//! Reading barcodes back with rxing (a port of ZXing): the stand-in for a scanner.

#![allow(dead_code)]

pub mod labels;

use quadra_label::Modules;
use rxing::{
    BarcodeFormat, DecodeHints, RXingResult, RXingResultMetadataType, RXingResultMetadataValue,
};

/// Pixels per module when rasterizing.
pub const PX: usize = 4;

/// A grayscale bitmap (0 = black, 255 = white) of modules with a quiet zone, `PX` pixels
/// per module; bars are 60 modules tall.
pub fn rasterize(modules: &Modules, quiet: usize) -> (Vec<u8>, usize, usize) {
    match modules {
        Modules::Linear { bars, .. } => {
            let w = (bars.len() + 2 * quiet) * PX;
            let h = 60 * PX;
            let mut img = vec![255u8; w * h];
            for y in 0..h {
                for (i, dark) in bars.iter().enumerate() {
                    if *dark {
                        let x0 = (quiet + i) * PX;
                        img[y * w + x0..y * w + x0 + PX].fill(0);
                    }
                }
            }
            (img, w, h)
        }
        Modules::Matrix {
            size_w,
            size_h,
            dark,
        } => {
            let w = (size_w + 2 * quiet) * PX;
            let h = (size_h + 2 * quiet) * PX;
            let mut img = vec![255u8; w * h];
            for my in 0..*size_h {
                for mx in 0..*size_w {
                    if dark[my * size_w + mx] {
                        for y in 0..PX {
                            let row = ((quiet + my) * PX + y) * w;
                            let x0 = (quiet + mx) * PX;
                            img[row + x0..row + x0 + PX].fill(0);
                        }
                    }
                }
            }
            (img, w, h)
        }
    }
}

/// Decodes one barcode of `format` from a grayscale bitmap, GS1 aware.
pub fn decode(
    img: Vec<u8>,
    w: usize,
    h: usize,
    format: BarcodeFormat,
) -> Result<RXingResult, String> {
    let mut hints = DecodeHints {
        AssumeGs1: Some(true),
        TryHarder: Some(true),
        ..Default::default()
    };
    rxing::helpers::detect_in_luma_with_hints(img, w as u32, h as u32, Some(format), &mut hints)
        .map_err(|e| format!("{e:?}"))
}

/// The symbology identifier rxing reports (`]C1`, `]d2`, ...), if any.
pub fn symbology_id(r: &RXingResult) -> Option<String> {
    match r
        .getRXingResultMetadata()
        .get(&RXingResultMetadataType::SYMBOLOGY_IDENTIFIER)
    {
        Some(RXingResultMetadataValue::SymbologyIdentifier(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Crops a region of a grayscale image (clamped to its bounds).
pub fn crop(
    img: &[u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    cw: usize,
    ch: usize,
) -> (Vec<u8>, usize, usize) {
    let x1 = (x + cw).min(w);
    let y1 = (y + ch).min(h);
    let mut out = Vec::with_capacity((x1 - x) * (y1 - y));
    for row in y..y1 {
        out.extend_from_slice(&img[row * w + x..row * w + x1]);
    }
    (out, x1 - x, y1 - y)
}
