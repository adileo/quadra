//! Checking a label and resolving its elements into what the ZPL and PDF writers draw.

use crate::font::{self, LINE_SPACING, PT_PER_MM};
use crate::image::{self, Rgba};
use crate::svg::HR_GAP_MM;
use crate::{encode, Align, Element, Label, Modules, Symbology};

/// Size of the human-readable line under bar symbols, in points.
pub(crate) const HR_PT: f64 = 8.0;

/// Tolerance on positions (millimetres), so that a value computed as 99.99999999 fits a
/// 100 mm label.
const EPS: f64 = 1e-6;

/// The resolutions of thermal printers this crate writes ZPL for.
pub(crate) const DPIS: [u32; 4] = [152, 203, 300, 600];

/// An element ready to be drawn. Lengths are millimetres from the label's top-left corner.
pub(crate) enum Item {
    Text {
        x: f64,
        y: f64,
        size_pt: f64,
        bold: bool,
        /// Width of the box the lines are aligned in.
        box_w: f64,
        align: Align,
        lines: Vec<String>,
    },
    Bars {
        x: f64,
        y: f64,
        module: f64,
        height: f64,
        bars: Vec<bool>,
        kind: Symbology,
        data: String,
        /// The human-readable line and the box it is centred in: (text, left, width).
        text: Option<(String, f64, f64)>,
    },
    Matrix {
        x: f64,
        y: f64,
        module: f64,
        /// The largest side allowed (`height_mm`).
        max_side: f64,
        fixed_module: bool,
        side: usize,
        dark: Vec<bool>,
        kind: Symbology,
        data: String,
    },
    Box {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        t: f64,
    },
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        t: f64,
    },
    Image {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        image: Rgba,
        png: Vec<u8>,
    },
}

fn short(s: &str) -> String {
    let mut out: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        out.push('…');
    }
    out
}

/// How messages name an element: "element 2 (text `Ship to`)".
pub(crate) fn describe(index: usize, e: &Element) -> String {
    let what = match e {
        Element::Text { text, .. } => format!("text `{}`", short(text)),
        Element::Barcode { kind, data, .. } => format!("{} `{}`", kind.name(), short(data)),
        Element::Box { .. } => "box".to_string(),
        Element::Line { .. } => "line".to_string(),
        Element::Image { .. } => "image".to_string(),
    };
    format!("element {} ({what})", index + 1)
}

fn positive(name: &str, v: f64) -> Result<(), String> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be more than 0, not {v}"))
    }
}

fn finite(name: &str, v: f64) -> Result<(), String> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(format!("{name} must be a number, not {v}"))
    }
}

/// Checks that a span [from, to] along one axis lies on the label.
fn inside(what: &str, from: f64, to: f64, axis: &str, size: f64) -> Result<(), String> {
    if from < -EPS {
        return Err(format!(
            "{what} starts {} mm {} the label",
            fmt_mm(-from),
            if axis == "x" { "left of" } else { "above" }
        ));
    }
    if to > size + EPS {
        return Err(format!(
            "{what} ends at {axis} = {} mm, past the {} of the label ({} mm)",
            fmt_mm(to),
            if axis == "x" { "right edge" } else { "bottom" },
            fmt_mm(size)
        ));
    }
    Ok(())
}

pub(crate) fn fmt_mm(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

/// Checks the label itself.
pub(crate) fn check_label(label: &Label) -> Result<(), String> {
    positive("the width", label.width_mm)?;
    positive("the height", label.height_mm)?;
    if !DPIS.contains(&label.dpi) {
        return Err(format!(
            "the resolution is {} dpi; it must be 152, 203, 300 or 600",
            label.dpi
        ));
    }
    Ok(())
}

/// Checks a label and resolves its elements. Errors name the label (1-based `number`) and
/// the element.
pub(crate) fn layout(number: usize, label: &Label) -> Result<Vec<Item>, String> {
    check_label(label).map_err(|e| format!("label {number}: {e}"))?;
    label
        .elements
        .iter()
        .enumerate()
        .map(|(i, e)| {
            item(label, e).map_err(|msg| format!("label {number}, {}: {msg}", describe(i, e)))
        })
        .collect()
}

fn item(label: &Label, e: &Element) -> Result<Item, String> {
    let (lw, lh) = (label.width_mm, label.height_mm);
    match e {
        Element::Text {
            x_mm,
            y_mm,
            size_pt,
            bold,
            width_mm,
            align,
            text,
        } => {
            finite("x", *x_mm)?;
            finite("y", *y_mm)?;
            positive("the text size", *size_pt)?;
            inside("the text", *x_mm, *x_mm, "x", lw)?;
            let box_w = match width_mm {
                Some(w) => {
                    positive("the text width", *w)?;
                    *w
                }
                None => lw - x_mm,
            };
            inside("the text box", *x_mm, x_mm + box_w, "x", lw)?;
            if let Some(c) = text
                .chars()
                .find(|c| *c != '\n' && font::winansi(*c).is_none())
            {
                return Err(format!(
                    "the label fonts cannot print `{c}` (U+{:04X}): text is limited to the \
Windows-1252 characters (Western European languages)",
                    c as u32
                ));
            }
            let lines = font::wrap(text, *size_pt, *bold, box_w);
            for line in &lines {
                let w = font::width_mm(&font::encode(line).unwrap_or_default(), *size_pt, *bold);
                if w > box_w + EPS {
                    return Err(format!(
                        "`{}` is {} mm wide at {} pt and does not fit the {} mm of the text box",
                        short(line),
                        fmt_mm(w),
                        size_pt,
                        fmt_mm(box_w)
                    ));
                }
            }
            let h = lines.len() as f64 * size_pt * LINE_SPACING / PT_PER_MM;
            inside(
                &format!("the text ({} lines)", lines.len()),
                *y_mm,
                y_mm + h,
                "y",
                lh,
            )?;
            Ok(Item::Text {
                x: *x_mm,
                y: *y_mm,
                size_pt: *size_pt,
                bold: *bold,
                box_w,
                align: *align,
                lines,
            })
        }
        Element::Barcode {
            x_mm,
            y_mm,
            kind,
            data,
            height_mm,
            module_mm,
            human_readable,
        } => {
            finite("x", *x_mm)?;
            finite("y", *y_mm)?;
            positive("the height", *height_mm)?;
            if let Some(m) = module_mm {
                positive("the module width", *m)?;
            }
            let qz = kind.quiet_zone() as f64;
            match encode(*kind, data)? {
                Modules::Linear { bars, text } => {
                    let m = module_mm.unwrap_or(kind.default_module_mm());
                    let w = bars.len() as f64 * m;
                    inside(
                        &format!("the barcode with its quiet zone ({} mm)", fmt_mm(qz * m)),
                        x_mm - qz * m,
                        x_mm + w + qz * m,
                        "x",
                        lw,
                    )?;
                    inside("the barcode", *y_mm, y_mm + height_mm, "y", lh)?;
                    let text = if *human_readable {
                        let shown: String = text
                            .chars()
                            .map(|c| if font::winansi(c).is_some() { c } else { ' ' })
                            .collect();
                        let tw = font::width_mm(&font::encode(&shown).unwrap(), HR_PT, false);
                        let bw = w.max(tw);
                        let left = x_mm + w / 2.0 - bw / 2.0;
                        let bottom = y_mm + height_mm + HR_GAP_MM + HR_PT / PT_PER_MM;
                        inside("the human-readable line", left, left + bw, "x", lw)?;
                        inside("the human-readable line", *y_mm, bottom, "y", lh)?;
                        Some((shown, left, bw))
                    } else {
                        None
                    };
                    Ok(Item::Bars {
                        x: *x_mm,
                        y: *y_mm,
                        module: m,
                        height: *height_mm,
                        bars,
                        kind: *kind,
                        data: data.clone(),
                        text,
                    })
                }
                Modules::Matrix { size_w, dark, .. } => {
                    let n = size_w as f64;
                    let m = match module_mm {
                        Some(m) => {
                            if n * m > height_mm + EPS {
                                return Err(format!(
                                    "the symbol has {size_w} modules per side: at {} mm per \
module it is {} mm, more than its height of {} mm",
                                    fmt_mm(*m),
                                    fmt_mm(n * m),
                                    fmt_mm(*height_mm)
                                ));
                            }
                            *m
                        }
                        None => height_mm / n,
                    };
                    let side = n * m;
                    let what = format!("the symbol with its quiet zone ({} mm)", fmt_mm(qz * m));
                    inside(&what, x_mm - qz * m, x_mm + side + qz * m, "x", lw)?;
                    inside(&what, y_mm - qz * m, y_mm + side + qz * m, "y", lh)?;
                    Ok(Item::Matrix {
                        x: *x_mm,
                        y: *y_mm,
                        module: m,
                        max_side: *height_mm,
                        fixed_module: module_mm.is_some(),
                        side: size_w,
                        dark,
                        kind: *kind,
                        data: data.clone(),
                    })
                }
            }
        }
        Element::Box {
            x_mm,
            y_mm,
            w_mm,
            h_mm,
            thickness_mm,
        } => {
            finite("x", *x_mm)?;
            finite("y", *y_mm)?;
            positive("the width", *w_mm)?;
            positive("the height", *h_mm)?;
            positive("the thickness", *thickness_mm)?;
            inside("the box", *x_mm, x_mm + w_mm, "x", lw)?;
            inside("the box", *y_mm, y_mm + h_mm, "y", lh)?;
            Ok(Item::Box {
                x: *x_mm,
                y: *y_mm,
                w: *w_mm,
                h: *h_mm,
                t: thickness_mm.min(w_mm / 2.0).min(h_mm / 2.0),
            })
        }
        Element::Line {
            x1_mm,
            y1_mm,
            x2_mm,
            y2_mm,
            thickness_mm,
        } => {
            for (name, v) in [("x1", x1_mm), ("y1", y1_mm), ("x2", x2_mm), ("y2", y2_mm)] {
                finite(name, *v)?;
            }
            positive("the thickness", *thickness_mm)?;
            if (x1_mm - x2_mm).abs() < EPS && (y1_mm - y2_mm).abs() < EPS {
                return Err("the line has no length: both ends are the same point".into());
            }
            inside("the line", x1_mm.min(*x2_mm), x1_mm.max(*x2_mm), "x", lw)?;
            inside("the line", y1_mm.min(*y2_mm), y1_mm.max(*y2_mm), "y", lh)?;
            Ok(Item::Line {
                x1: *x1_mm,
                y1: *y1_mm,
                x2: *x2_mm,
                y2: *y2_mm,
                t: *thickness_mm,
            })
        }
        Element::Image {
            x_mm,
            y_mm,
            w_mm,
            h_mm,
            png,
        } => {
            finite("x", *x_mm)?;
            finite("y", *y_mm)?;
            positive("the width", *w_mm)?;
            positive("the height", *h_mm)?;
            inside("the image", *x_mm, x_mm + w_mm, "x", lw)?;
            inside("the image", *y_mm, y_mm + h_mm, "y", lh)?;
            let image = image::decode(png)?;
            Ok(Item::Image {
                x: *x_mm,
                y: *y_mm,
                w: *w_mm,
                h: *h_mm,
                image,
                png: png.clone(),
            })
        }
    }
}
