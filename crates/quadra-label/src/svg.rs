//! One symbol as SVG, for documents.

use crate::font::PT_PER_MM;
use crate::{encode, Modules, Symbology};

/// How [`svg`] draws a symbol. Lengths are millimetres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvgOptions {
    /// Width of one module (the narrowest bar, or one square of a matrix symbol).
    pub module_mm: f64,
    /// Height of the bars; matrix symbols are square and ignore it.
    pub height_mm: f64,
    /// Quiet zone on each side, in modules; `None` is what the standard asks for
    /// ([`Symbology::quiet_zone`]). Bar symbols get it left and right, matrix symbols all
    /// around.
    pub quiet_zone: Option<u32>,
    /// Print the human-readable line under bar symbols.
    pub human_readable: bool,
    /// Size of the human-readable line, in points.
    pub font_size_pt: f64,
}

impl Default for SvgOptions {
    /// 0.33 mm modules, 15 mm bars, the standard quiet zone, human-readable line at 8 pt.
    fn default() -> Self {
        SvgOptions {
            module_mm: 0.33,
            height_mm: 15.0,
            quiet_zone: None,
            human_readable: true,
            font_size_pt: 8.0,
        }
    }
}

/// A number with three decimals, the way every length is written.
pub(crate) fn num(v: f64) -> String {
    let s = format!("{v:.3}");
    if s == "-0.000" {
        "0.000".into()
    } else {
        s
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 32 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The runs of dark modules in a row: (start, length).
pub(crate) fn runs(row: &[bool]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        if row[i] {
            let start = i;
            while i < row.len() && row[i] {
                i += 1;
            }
            out.push((start, i - start));
        } else {
            i += 1;
        }
    }
    out
}

/// A symbol as a small SVG document: a white background, one black `<rect>` per run of
/// dark modules (adjacent dark modules of a row are merged), and the human-readable line
/// when asked for. The user unit is the millimetre (`viewBox` in mm, `width` and `height`
/// with `mm` units) and every number has three decimals, so the output is the same bytes
/// on every machine.
///
/// ```
/// use quadra_label::{svg, Symbology, SvgOptions};
///
/// let doc = svg(Symbology::Code128, "ABC-123", &SvgOptions::default()).unwrap();
/// assert!(doc.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
/// ```
pub fn svg(kind: Symbology, data: &str, opts: &SvgOptions) -> Result<String, String> {
    let m = opts.module_mm;
    if !(m.is_finite() && m > 0.0) {
        return Err(format!("the module width must be more than 0 mm, not {m}"));
    }
    let modules = encode(kind, data)?;
    let qz = opts.quiet_zone.unwrap_or(kind.quiet_zone()) as f64 * m;
    let mut body = String::new();
    let (w, h);
    match &modules {
        Modules::Linear { bars, text } => {
            let bar_h = opts.height_mm;
            if !(bar_h.is_finite() && bar_h > 0.0) {
                return Err(format!(
                    "the bar height must be more than 0 mm, not {bar_h}"
                ));
            }
            w = bars.len() as f64 * m + 2.0 * qz;
            let font_mm = opts.font_size_pt / PT_PER_MM;
            let show_text = opts.human_readable && font_mm > 0.0;
            h = if show_text {
                bar_h + HR_GAP_MM + font_mm
            } else {
                bar_h
            };
            for (start, len) in runs(bars) {
                body.push_str(&format!(
                    "<rect x=\"{}\" y=\"0.000\" width=\"{}\" height=\"{}\"/>\n",
                    num(qz + start as f64 * m),
                    num(len as f64 * m),
                    num(bar_h)
                ));
            }
            if show_text {
                body.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" font-family=\"Helvetica, Arial, sans-serif\" \
font-size=\"{}\" text-anchor=\"middle\">{}</text>\n",
                    num(w / 2.0),
                    num(bar_h + HR_GAP_MM + 0.8 * font_mm),
                    num(font_mm),
                    escape(text)
                ));
            }
        }
        Modules::Matrix {
            size_w,
            size_h,
            dark,
        } => {
            w = *size_w as f64 * m + 2.0 * qz;
            h = *size_h as f64 * m + 2.0 * qz;
            for y in 0..*size_h {
                let row = &dark[y * size_w..(y + 1) * size_w];
                for (start, len) in runs(row) {
                    body.push_str(&format!(
                        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>\n",
                        num(qz + start as f64 * m),
                        num(qz + y as f64 * m),
                        num(len as f64 * m),
                        num(m)
                    ));
                }
            }
        }
    }
    let (w, h) = (num(w), num(h));
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}mm\" height=\"{h}mm\" \
viewBox=\"0 0 {w} {h}\" shape-rendering=\"crispEdges\">\n\
<rect width=\"{w}\" height=\"{h}\" fill=\"#ffffff\"/>\n\
<g fill=\"#000000\">\n{body}</g>\n</svg>\n"
    ))
}

/// Space between the bars and the human-readable line, in millimetres.
pub(crate) const HR_GAP_MM: f64 = 0.5;
