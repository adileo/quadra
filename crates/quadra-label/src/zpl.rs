//! ZPL II for Zebra thermal printers.

use std::fmt::Write as _;

use crate::code128::{self, Set, Token};
use crate::font::LINE_SPACING;
use crate::gs1::{self, Unit};
use crate::layout::{self, describe, fmt_mm, Item, HR_PT};
use crate::svg::HR_GAP_MM;
use crate::{code128_units, datamatrix_units, ean, Align, Label, QrEcc, Symbology};

/// Millimetres to dots at a resolution, rounded to the nearest dot (halves away from zero).
fn dots(mm: f64, dpi: u32) -> i64 {
    (mm * dpi as f64 / 25.4).round() as i64
}

/// The smallest character height font 0 accepts, in dots.
const MIN_FONT_DOTS: i64 = 10;

/// Field data escaped for `^FH` (hexadecimal indicator `_`): `^`, `~`, `_` and control
/// characters become `_5E`, `_7E`, `_5F`, `_0D`... Returns the `^FH` command (empty when
/// nothing needs escaping) and the data.
fn field(data: &str) -> (&'static str, String) {
    let needs = data
        .chars()
        .any(|c| c == '^' || c == '~' || (c as u32) < 32);
    if !needs {
        return ("", data.to_string());
    }
    let mut out = String::with_capacity(data.len() + 8);
    for c in data.chars() {
        match c {
            '^' | '~' | '_' => {
                let _ = write!(out, "_{:02X}", c as u32);
            }
            c if (c as u32) < 32 => {
                let _ = write!(out, "_{:02X}", c as u32);
            }
            c => out.push(c),
        }
    }
    ("^FH", out)
}

fn ecc_letter(ecc: QrEcc) -> char {
    match ecc {
        QrEcc::L => 'L',
        QrEcc::M => 'M',
        QrEcc::Q => 'Q',
        QrEcc::H => 'H',
    }
}

/// The `^FD` data of a Code 128 plan in `^BC` mode N: explicit start (`>9` A, `>:` B, `>;` C),
/// code set changes (`>5` to C, `>6` to B, `>7` to A), FNC1 (`>8`), digit pairs in code set
/// C, and `><` for `>` in code sets A and B.
fn bc_data(tokens: &[Token]) -> (&'static str, String) {
    let mut raw = String::new();
    let mut needs_fh = false;
    let mut chars = String::new();
    for t in tokens {
        if let Token::Char(b) = t {
            chars.push(*b as char);
        }
    }
    if chars
        .chars()
        .any(|c| c == '^' || c == '~' || (c as u32) < 32)
    {
        needs_fh = true;
    }
    for t in tokens {
        match *t {
            Token::Start(Set::A) => raw.push_str(">9"),
            Token::Start(Set::B) => raw.push_str(">:"),
            Token::Start(Set::C) => raw.push_str(">;"),
            Token::Switch(Set::C) => raw.push_str(">5"),
            Token::Switch(Set::B) => raw.push_str(">6"),
            Token::Switch(Set::A) => raw.push_str(">7"),
            Token::Fnc1 => raw.push_str(">8"),
            Token::Pair(p) => {
                let _ = write!(raw, "{p:02}");
            }
            Token::Char(b'>') => raw.push_str("><"),
            Token::Char(b) => {
                let c = b as char;
                if needs_fh && (c == '^' || c == '~' || c == '_' || b < 32) {
                    let _ = write!(raw, "_{b:02X}");
                } else {
                    raw.push(c);
                }
            }
        }
    }
    (if needs_fh { "^FH" } else { "" }, raw)
}

/// A character that does not occur in `data` (and is not `skip`), for escapes.
fn unused(data: &str, skip: Option<char>) -> Result<char, String> {
    ['_', '|', '!', '#', '$', '@', '`', '{', '}']
        .into_iter()
        .find(|c| !data.contains(*c) && Some(*c) != skip)
        .ok_or_else(|| "the data uses every character ZPL could take as escape character".into())
}

/// The ZPL of labels, one `^XA … ^XZ` format per label, one command per line.
///
/// Positions and sizes are converted from millimetres to dots at the label's `dpi`:
/// `dots = round(mm × dpi / 25.4)`, halves rounded away from zero (100 mm at 203 dpi is
/// 799.2 dots, so 799). Widths and heights are the difference between the rounded far
/// edge and the rounded near edge, so adjacent elements stay adjacent.
///
/// Each label is:
///
/// - `^XA`, `^CI28` (field data is UTF-8), `^PW` (print width) and `^LL` (label length)
///   in dots, `^LH0,0`, the elements in order, `^XZ`.
/// - Text: `^FOx,y^A0N,h,h^FBw,lines,spacing,J,0^FD…^FS`: font 0 (the printer's scalable
///   font) with height `h` = the size in points at the dpi (at least 10 dots), a field block
///   as wide as the text box with the lines wrapped as in the PDF (joined with `\&`), line
///   spacing 15% of `h`, justification `L`, `C` or `R`. Font 0 has a single weight, so
///   `bold` does not change the ZPL.
/// - Code 128, GS1-128 and SSCC: `^FOx,y^BYm^BCN,h,N,N,N,N^FD…^FS`: module width `m` dots
///   from `module_mm` (at least 2, at most 10, the limits of `^BY`), mode N with the exact
///   code sets of [`crate::encode`]: `>;` start C, `>:` start B, `>9` start A, `>5`/`>6`/`>7`
///   change to C/B/A, `>8` FNC1, `><` for `>`. GS1 data is `^FD>;>8…`: start C then FNC1.
/// - EAN-13: `^FOx,y^BYm^BEN,h,N,N^FD` + the first 12 digits `^FS` (the printer adds the
///   check digit).
/// - The human-readable line of bar symbols is a separate centred text field under the bars
///   (`^A0N` at 8 pt, `^FB…,1,0,C,0`), so it reads `(00)…` with parentheses as in the PDF.
/// - QR: `^FOx,y^BQN,2,m,E^FDEA,…^FS`: model 2, magnification `m` (1 to 10 dots per
///   module), error correction `E` (L, M, Q, H), automatic input mode.
/// - Data Matrix: `^FOx,y^BXN,m,200,n,n,,e,1^FD…^FS`: ECC 200, `m` dots per module, `n`×`n`
///   modules (the size [`crate::encode`] chose), escape character `e` (`_`, or the first of
///   `|!#$@` that is not in the data), square. GS1 data is `e1` (FNC1) first, then the
///   element strings with `e1` after each variable-length one. Other data with `^`, `~` or
///   control characters gets `^FHi` with an indicator `i` that is neither in the data nor
///   `e` (`^FH!` and `!5E` when `e` is `|`).
/// - Box: `^FOx,y^GBw,h,t,B,0^FS` (the line is inside the box).
/// - Line: `^GBlength,t,t,B,0` when horizontal, `^GBt,length,t,B,0` when vertical,
///   `^GDw,h,t,B,L` or `R` when diagonal (`L` leans like `\`, `R` like `/`).
/// - Image: `^FOx,y^GFA,n,n,r,HEX^FS`: the PNG scaled to the box in dots, black where its
///   luminance over white is below 50%, `r` bytes per row, uppercase hexadecimal.
/// - Field data with `^`, `~` or control characters gets `^FH` and `_5E`, `_7E`, `_XX`
///   escapes (and `_5F` for `_`).
///
/// ```
/// use quadra_label::{to_zpl, Element, Label};
///
/// let label = Label {
///     width_mm: 100.0,
///     height_mm: 150.0,
///     dpi: 203,
///     elements: vec![Element::Box { x_mm: 5.0, y_mm: 5.0, w_mm: 90.0, h_mm: 140.0, thickness_mm: 0.5 }],
/// };
/// assert_eq!(
///     to_zpl(&[label]).unwrap(),
///     "^XA\n^CI28\n^PW799\n^LL1199\n^LH0,0\n^FO40,40^GB719,1119,4,B,0^FS\n^XZ\n"
/// );
/// ```
pub fn to_zpl(labels: &[Label]) -> Result<String, String> {
    if labels.is_empty() {
        return Err("there are no labels".into());
    }
    let mut out = String::new();
    for (i, label) in labels.iter().enumerate() {
        let items = layout::layout(i + 1, label)?;
        let dpi = label.dpi;
        let d = |mm: f64| dots(mm, dpi);
        let (pw, ll) = (d(label.width_mm), d(label.height_mm));
        let _ = write!(out, "^XA\n^CI28\n^PW{pw}\n^LL{ll}\n^LH0,0\n");
        for (k, item) in items.iter().enumerate() {
            let fail = |msg: String| {
                format!(
                    "label {}, {}: {msg}",
                    i + 1,
                    describe(k, &label.elements[k])
                )
            };
            write_item(&mut out, item, dpi, pw, ll).map_err(fail)?;
        }
        out.push_str("^XZ\n");
    }
    Ok(out)
}

fn font_dots(size_pt: f64, dpi: u32) -> i64 {
    ((size_pt / 72.0 * dpi as f64).round() as i64).max(MIN_FONT_DOTS)
}

fn text_field(
    out: &mut String,
    x: i64,
    y: i64,
    h: i64,
    box_w: i64,
    align: Align,
    lines: &[String],
) {
    let spacing = ((h as f64) * (LINE_SPACING - 1.0)).round() as i64;
    let joined = lines
        .iter()
        .map(|l| l.replace('\\', "\\\\"))
        .collect::<Vec<_>>()
        .join("\\&");
    let (fh, data) = field(&joined);
    let j = match align {
        Align::Left => 'L',
        Align::Center => 'C',
        Align::Right => 'R',
    };
    let _ = writeln!(
        out,
        "^FO{x},{y}^A0N,{h},{h}^FB{box_w},{},{spacing},{j},0{fh}^FD{data}^FS",
        lines.len()
    );
}

fn write_item(out: &mut String, item: &Item, dpi: u32, pw: i64, ll: i64) -> Result<(), String> {
    let d = |mm: f64| dots(mm, dpi);
    match item {
        Item::Text {
            x,
            y,
            size_pt,
            box_w,
            align,
            lines,
            ..
        } => {
            if lines.iter().all(|l| l.is_empty()) {
                return Ok(());
            }
            let h = font_dots(*size_pt, dpi);
            text_field(out, d(*x), d(*y), h, d(x + box_w) - d(*x), *align, lines);
        }
        Item::Bars {
            x,
            y,
            module,
            height,
            bars,
            kind,
            data,
            text,
        } => {
            let m = d(*module).clamp(2, 10);
            let (x0, y0) = (d(*x), d(*y));
            let h = d(y + height) - y0;
            let qz = kind.quiet_zone() as i64 * m;
            let w = bars.len() as i64 * m;
            if x0 - qz < 0 || x0 + w + qz > pw {
                return Err(format!(
                    "at {dpi} dpi the module is {m} dots ({} mm) and the barcode with its quiet \
zone takes {} dots from x = {} dots, more than the label's {pw} dots",
                    fmt_mm(m as f64 * 25.4 / dpi as f64),
                    w + 2 * qz,
                    x0 - qz
                ));
            }
            let _ = write!(out, "^FO{x0},{y0}^BY{m}");
            if *kind == Symbology::Ean13 {
                let digits = ean::digits(data)?;
                let _ = writeln!(out, "^BEN,{h},N,N^FD{}^FS", &digits[..12]);
            } else {
                let (units, _) = code128_units(*kind, data)?;
                let tokens = code128::plan(&units)?;
                let (fh, fd) = bc_data(&tokens);
                let _ = writeln!(out, "^BCN,{h},N,N,N,N{fh}^FD{fd}^FS");
            }
            if let Some((shown, left, bw)) = text {
                let th = font_dots(HR_PT, dpi);
                let ty = d(y + height + HR_GAP_MM);
                if ty + th > ll {
                    return Err(format!(
                        "at {dpi} dpi the human-readable line ends at {} dots, past the \
label's {ll} dots",
                        ty + th
                    ));
                }
                let lines = [shown.clone()];
                text_field(
                    out,
                    d(*left),
                    ty,
                    th,
                    d(left + bw) - d(*left),
                    Align::Center,
                    &lines,
                );
            }
        }
        Item::Matrix {
            x,
            y,
            module,
            max_side,
            fixed_module,
            side,
            kind,
            data,
            ..
        } => {
            let n = *side as i64;
            let m = if *fixed_module {
                d(*module).max(1)
            } else {
                d(*max_side) / n
            };
            if m < 1 {
                return Err(format!(
                    "at {dpi} dpi the symbol needs at least {n} dots ({} mm) for its {n} \
modules per side; its height is {} mm",
                    fmt_mm(n as f64 * 25.4 / dpi as f64),
                    fmt_mm(*max_side)
                ));
            }
            let (x0, y0) = (d(*x), d(*y));
            match kind {
                Symbology::Qr(ecc) => {
                    let m = m.min(10);
                    let e = ecc_letter(*ecc);
                    let (fh, fd) = field(data);
                    let _ = writeln!(out, "^FO{x0},{y0}^BQN,2,{m},{e}{fh}^FD{e}A,{fd}^FS");
                    check_matrix(x0, y0, n, m, 4, pw, ll, dpi)?;
                }
                _ => {
                    check_matrix(x0, y0, n, m, 1, pw, ll, dpi)?;
                    let esc = unused(data, None)?;
                    let (fh, fd) = dm_data(data, esc)?;
                    let _ = writeln!(
                        out,
                        "^FO{x0},{y0}^BXN,{m},200,{n},{n},,{esc},1{fh}^FD{fd}^FS"
                    );
                }
            }
        }
        Item::Box { x, y, w, h, t } => {
            let (x0, y0) = (d(*x), d(*y));
            let _ = writeln!(
                out,
                "^FO{x0},{y0}^GB{},{},{},B,0^FS",
                d(x + w) - x0,
                d(y + h) - y0,
                d(*t).max(1)
            );
        }
        Item::Line { x1, y1, x2, y2, t } => {
            let td = d(*t).max(1);
            let (lx, rx) = (x1.min(*x2), x1.max(*x2));
            let (ty, by) = (y1.min(*y2), y1.max(*y2));
            if (y1 - y2).abs() < 1e-9 {
                let x0 = d(lx);
                let _ = writeln!(
                    out,
                    "^FO{x0},{}^GB{},{td},{td},B,0^FS",
                    d(y1 - t / 2.0),
                    d(rx) - x0
                );
            } else if (x1 - x2).abs() < 1e-9 {
                let y0 = d(ty);
                let _ = writeln!(
                    out,
                    "^FO{},{y0}^GB{td},{},{td},B,0^FS",
                    d(x1 - t / 2.0),
                    d(by) - y0
                );
            } else {
                let lean = if (x2 - x1) * (y2 - y1) > 0.0 {
                    'L'
                } else {
                    'R'
                };
                let (x0, y0) = (d(lx), d(ty));
                let _ = writeln!(
                    out,
                    "^FO{x0},{y0}^GD{},{},{td},B,{lean}^FS",
                    d(rx) - x0,
                    d(by) - y0
                );
            }
        }
        Item::Image {
            x, y, w, h, image, ..
        } => {
            let (x0, y0) = (d(*x), d(*y));
            let (wd, hd) = ((d(x + w) - x0).max(1), (d(y + h) - y0).max(1));
            let (row, bits) = image.to_1bit(wd as usize, hd as usize);
            let total = bits.len();
            let _ = write!(out, "^FO{x0},{y0}^GFA,{total},{total},{row},");
            for b in bits {
                let _ = write!(out, "{b:02X}");
            }
            out.push_str("^FS\n");
        }
    }
    Ok(())
}

/// The `^FH` command (if any) and the `^FD` data of a Data Matrix whose escape character is
/// `esc`: GS1 data is `esc1` (FNC1) and the element strings; other data is as is, with `^`,
/// `~` and control characters as hexadecimal escapes after an indicator that is neither in
/// the data nor `esc`.
fn dm_data(data: &str, esc: char) -> Result<(String, String), String> {
    datamatrix_units(data)?;
    let mut s = String::new();
    if crate::is_gs1_datamatrix(data) {
        for u in gs1::units(&gs1::parse(data)?) {
            match u {
                Unit::Fnc1 => {
                    s.push(esc);
                    s.push('1');
                }
                Unit::Byte(b) => s.push(b as char),
            }
        }
        return Ok((String::new(), s));
    }
    let special = |c: char| c == '^' || c == '~' || (c as u32) < 32;
    if !data.chars().any(special) {
        return Ok((String::new(), data.to_string()));
    }
    let ind = unused(data, Some(esc))?;
    for c in data.chars() {
        if special(c) {
            let _ = write!(s, "{ind}{:02X}", c as u32);
        } else {
            s.push(c);
        }
    }
    Ok((format!("^FH{ind}"), s))
}

#[allow(clippy::too_many_arguments)]
fn check_matrix(
    x0: i64,
    y0: i64,
    n: i64,
    m: i64,
    qz: i64,
    pw: i64,
    ll: i64,
    dpi: u32,
) -> Result<(), String> {
    let side = n * m;
    if x0 - qz * m < 0 || y0 - qz * m < 0 || x0 + side + qz * m > pw || y0 + side + qz * m > ll {
        return Err(format!(
            "at {dpi} dpi the symbol is {side} dots per side ({m} per module) and with its quiet \
zone it does not fit the label ({pw} × {ll} dots)"
        ));
    }
    Ok(())
}
