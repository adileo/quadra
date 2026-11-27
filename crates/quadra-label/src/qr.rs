//! QR Code model 2 through the `qrcode` crate, with the segmentation fixed for UTF-8 text.

use qrcode::bits::Bits;
use qrcode::optimize::{total_encoded_len, Optimizer, Parser, Segment};
use qrcode::types::{Color, Mode, Version};
use qrcode::{EcLevel, QrCode};

use crate::QrEcc;

/// The smallest QR code that holds `data` at the error correction level. Numeric,
/// alphanumeric and byte segments are chosen to make the code as small as possible; the
/// Kanji mode is never used, because UTF-8 bytes can look like Shift JIS and would then be
/// read back as other characters.
pub(crate) fn encode(data: &str, ecc: QrEcc) -> Result<(usize, Vec<bool>), String> {
    if data.is_empty() {
        return Err("a QR code needs at least one character".into());
    }
    let level = match ecc {
        QrEcc::L => EcLevel::L,
        QrEcc::M => EcLevel::M,
        QrEcc::Q => EcLevel::Q,
        QrEcc::H => EcLevel::H,
    };
    let bytes = data.as_bytes();
    let segments: Vec<Segment> = Parser::new(bytes)
        .map(|s| {
            if s.mode == Mode::Kanji {
                Segment {
                    mode: Mode::Byte,
                    ..s
                }
            } else {
                s
            }
        })
        .collect();
    // The whole data in the one mode that can hold all of it: the optimizer merges segments
    // pair by pair and can miss that this is shorter.
    let alphanumeric =
        |b: &u8| b.is_ascii_digit() || b.is_ascii_uppercase() || b" $%*+-./:".contains(b);
    let single = [Segment {
        mode: if bytes.iter().all(u8::is_ascii_digit) {
            Mode::Numeric
        } else if bytes.iter().all(alphanumeric) {
            Mode::Alphanumeric
        } else {
            Mode::Byte
        },
        begin: 0,
        end: bytes.len(),
    }];
    for v in 1..=40 {
        let version = Version::Normal(v);
        let optimized: Vec<Segment> = Optimizer::new(segments.iter().copied(), version).collect();
        let best = if total_encoded_len(&single, version) <= total_encoded_len(&optimized, version)
        {
            single.to_vec()
        } else {
            optimized
        };
        let mut bits = Bits::new(version);
        if bits.push_segments(bytes, best.into_iter()).is_err()
            || bits.push_terminator(level).is_err()
        {
            continue;
        }
        let code = QrCode::with_bits(bits, level).map_err(|e| format!("QR: {e}"))?;
        let dark = code
            .to_colors()
            .into_iter()
            .map(|c| c == Color::Dark)
            .collect();
        return Ok((code.width(), dark));
    }
    Err(format!(
        "the data is too long for a QR code at error correction {ecc:?}: {} bytes",
        bytes.len()
    ))
}
