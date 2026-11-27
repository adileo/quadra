//! PNG images: decoding to RGBA, and the 1-bit version printed by thermal printers.

/// A decoded image, 8 bits per channel, RGBA, row-major.
#[derive(Clone, Debug)]
pub(crate) struct Rgba {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// The most memory a decoded image may take.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Decodes a PNG (any colour type and bit depth; 16-bit channels are reduced to 8).
pub(crate) fn decode(png_bytes: &[u8]) -> Result<Rgba, String> {
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(png_bytes),
        png::Limits { bytes: MAX_BYTES },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("the image is not a PNG that can be read: {e}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or("the image is too large")?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("the image is not a PNG that can be read: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 {
        return Err("the image is empty".into());
    }
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("the image palette could not be expanded".into()),
    };
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &buf[y * info.line_size..y * info.line_size + w * channels];
        for p in row.chunks_exact(channels) {
            let rgba = match channels {
                1 => [p[0], p[0], p[0], 255],
                2 => [p[0], p[0], p[0], p[1]],
                3 => [p[0], p[1], p[2], 255],
                _ => [p[0], p[1], p[2], p[3]],
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    Ok(Rgba {
        width: w,
        height: h,
        pixels,
    })
}

impl Rgba {
    /// Whether any pixel is not fully opaque.
    pub(crate) fn has_alpha(&self) -> bool {
        self.pixels.chunks_exact(4).any(|p| p[3] != 255)
    }

    /// Scaled to `w` × `h` dots (nearest pixel), composed over white and thresholded at
    /// 50% luminance (ITU-R BT.601 weights): one bit per dot, 1 = black, rows padded to whole
    /// bytes. Returns the bytes per row and the bytes.
    pub(crate) fn to_1bit(&self, w: usize, h: usize) -> (usize, Vec<u8>) {
        let row_bytes = w.div_ceil(8);
        let mut out = vec![0u8; row_bytes * h];
        for y in 0..h {
            let sy = ((2 * y + 1) * self.height) / (2 * h);
            for x in 0..w {
                let sx = ((2 * x + 1) * self.width) / (2 * w);
                let p = &self.pixels[(sy * self.width + sx) * 4..][..4];
                let a = p[3] as u32;
                let over_white = |c: u8| (c as u32 * a + 255 * (255 - a)) / 255;
                let lum =
                    (299 * over_white(p[0]) + 587 * over_white(p[1]) + 114 * over_white(p[2]))
                        / 1000;
                if lum < 128 {
                    out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        (row_bytes, out)
    }
}
