//! A spreadsheet of an app as an Excel file (`GET /v1/apps/{id}/export.xlsx`): the values the
//! user sees (evaluated as them), typed — numbers as numbers, dates as dates, amounts as
//! numbers with their currency in the number format —, the styles, merges, widths, heights and
//! frozen panes, one worksheet per sheet. A formula is exported as its value, with its Quadra
//! text in a note of the cell (Excel cannot compute it).

use super::sheets::{parse_a1, Out};
use quadra_runtime::{Interp, V};
use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, FormatPattern, FormatUnderline, Note, Workbook};
use serde_json::Value as J;
use std::collections::BTreeMap;

/// An Excel sheet name: at most 31 characters, none of `[]:*?/\`, unique.
fn sheet_name(name: &str, taken: &mut Vec<String>) -> String {
    let mut n: String = name.chars().map(|c| if "[]:*?/\\".contains(c) { '_' } else { c }).collect();
    n = n.trim_matches('\'').chars().take(31).collect();
    if n.trim().is_empty() {
        n = "Sheet".into();
    }
    let base = n.clone();
    let mut k = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&n)) {
        let suffix = format!(" ({k})");
        n = format!("{}{suffix}", base.chars().take(31 - suffix.len()).collect::<String>());
        k += 1;
    }
    taken.push(n.clone());
    n
}

fn color(s: &J) -> Option<Color> {
    let t = s.as_str()?.strip_prefix('#')?;
    if t.len() != 6 {
        return None;
    }
    u32::from_str_radix(t, 16).ok().map(Color::RGB)
}

fn border(s: &J) -> Option<FormatBorder> {
    Some(match s.as_str()? {
        "thin" => FormatBorder::Thin,
        "medium" => FormatBorder::Medium,
        "thick" => FormatBorder::Thick,
        _ => return None,
    })
}

/// The number format of a value: from the cell's `fmt` when it has one, else from the value
/// (an amount shows its currency, a quantity its unit, a date as a date).
fn num_format(fmt: &J, v: Option<&V>) -> Option<String> {
    let dec = fmt["dec"].as_u64().map(|d| d.min(10) as usize);
    let thou = fmt["thou"].as_bool().unwrap_or(false);
    let digits = |d: usize| {
        if d == 0 {
            String::new()
        } else {
            format!(".{}", "0".repeat(d))
        }
    };
    let number = |d: usize| format!("{}0{}", if thou { "#,##" } else { "" }, digits(d));
    let quoted = |u: &str| format!("\"{}\"", u.replace('"', ""));
    match fmt["kind"].as_str() {
        Some("number") => return Some(number(dec.unwrap_or(2))),
        Some("percent") => return Some(format!("0{}%", digits(dec.unwrap_or(0)))),
        Some("currency") => {
            let cur = fmt["cur"].as_str().map(str::to_string).or_else(|| match v {
                Some(V::Money { cur, .. }) => Some(cur.to_string()),
                _ => None,
            });
            return Some(match cur {
                Some(c) => format!("{} {}", number(dec.unwrap_or(2)), quoted(&c)),
                None => number(dec.unwrap_or(2)),
            });
        }
        Some("date") => {
            return Some(
                match fmt["date"].as_str().unwrap_or("yyyy-mm-dd") {
                    "dd/mm/yyyy" => "dd/mm/yyyy",
                    "mm/dd/yyyy" => "mm/dd/yyyy",
                    "d mmm yyyy" => "d mmm yyyy",
                    _ => "yyyy-mm-dd",
                }
                .to_string(),
            )
        }
        Some("text") => return Some("@".into()),
        _ => {}
    }
    match v? {
        V::Money { cur, .. } => Some(format!("{} {}", number(dec.unwrap_or(2)), quoted(cur))),
        V::Qty { unit, .. } => Some(format!("0.###### {}", quoted(unit))),
        V::Price { cur, per, .. } => Some(format!("{} {}", number(dec.unwrap_or(2)), quoted(&format!("{cur}/{per}")))),
        V::Pct(_) => Some(format!("0{}%", digits(dec.unwrap_or(0)))),
        V::Date(_) => Some("yyyy-mm-dd".into()),
        V::DateTime(_) => Some("yyyy-mm-dd hh:mm".into()),
        _ => dec.map(number),
    }
}

/// The format of a cell: its style, and the number format of its value.
fn format_of(style: &J, v: Option<&V>) -> Format {
    let mut f = Format::new();
    if style["b"] == J::Bool(true) {
        f = f.set_bold();
    }
    if style["i"] == J::Bool(true) {
        f = f.set_italic();
    }
    if style["u"] == J::Bool(true) {
        f = f.set_underline(FormatUnderline::Single);
    }
    if style["st"] == J::Bool(true) {
        f = f.set_font_strikethrough();
    }
    if let Some(c) = color(&style["fg"]) {
        f = f.set_font_color(c);
    }
    if let Some(c) = color(&style["bg"]) {
        f = f.set_pattern(FormatPattern::Solid).set_background_color(c);
    }
    match style["ha"].as_str() {
        Some("left") => f = f.set_align(FormatAlign::Left),
        Some("center") => f = f.set_align(FormatAlign::Center),
        Some("right") => f = f.set_align(FormatAlign::Right),
        _ => {}
    }
    match style["va"].as_str() {
        Some("top") => f = f.set_align(FormatAlign::Top),
        Some("middle") => f = f.set_align(FormatAlign::VerticalCenter),
        Some("bottom") => f = f.set_align(FormatAlign::Bottom),
        _ => {}
    }
    if style["wrap"] == J::Bool(true) {
        f = f.set_text_wrap();
    }
    let bd = &style["bd"];
    let bc = color(&bd["c"]);
    if let Some(b) = border(&bd["t"]) {
        f = f.set_border_top(b);
        if let Some(c) = bc {
            f = f.set_border_top_color(c);
        }
    }
    if let Some(b) = border(&bd["b"]) {
        f = f.set_border_bottom(b);
        if let Some(c) = bc {
            f = f.set_border_bottom_color(c);
        }
    }
    if let Some(b) = border(&bd["l"]) {
        f = f.set_border_left(b);
        if let Some(c) = bc {
            f = f.set_border_left_color(c);
        }
    }
    if let Some(b) = border(&bd["r"]) {
        f = f.set_border_right(b);
        if let Some(c) = bc {
            f = f.set_border_right_color(c);
        }
    }
    if let Some(nf) = num_format(&style["fmt"], v) {
        f = f.set_num_format(nf);
    }
    f
}

fn number(d: &rust_decimal::Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}

/// The workbook of a component (its `config`) with what each cell shows, as an `.xlsx` file.
/// `grid`: per sheet, (row, col) → the result of a formula or of a spilled cell.
pub fn workbook(cfg: &J, grid: &[BTreeMap<(u32, u32), Out>], i: &mut Interp) -> Result<Vec<u8>, String> {
    let e = |x: rust_xlsxwriter::XlsxError| x.to_string();
    let mut wb = Workbook::new();
    let mut taken = Vec::new();
    let sheets = cfg["sheets"].as_array().cloned().unwrap_or_default();
    for (k, s) in sheets.iter().enumerate() {
        let ws = wb.add_worksheet();
        ws.set_name(sheet_name(s["name"].as_str().unwrap_or("Sheet"), &mut taken)).map_err(e)?;
        let inputs = s["cells"].as_object().cloned().unwrap_or_default();
        let empty = BTreeMap::new();
        let results = grid.get(k).unwrap_or(&empty);
        // every cell with an input or a result
        let mut cells: BTreeMap<(u32, u32), (Option<&J>, Option<&Out>)> = BTreeMap::new();
        for (a, c) in &inputs {
            if let Some(rc) = parse_a1(a) {
                cells.entry(rc).or_default().0 = Some(c);
            }
        }
        for (rc, o) in results {
            cells.entry(*rc).or_default().1 = Some(o);
        }
        let merges: Vec<(u32, u32, u32, u32)> = s["merges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let (a, b) = m.as_str()?.split_once(':')?;
                let (r1, c1) = parse_a1(a)?;
                let (r2, c2) = parse_a1(b)?;
                Some((r1.min(r2), c1.min(c2), r1.max(r2), c1.max(c2)))
            })
            .collect();
        for (r1, c1, r2, c2) in &merges {
            if (r1, c1) != (r2, c2) {
                let style = inputs.get(&super::sheets::a1(*r1, *c1)).map(|c| c["s"].clone()).unwrap_or(J::Null);
                ws.merge_range(r1 - 1, (c1 - 1) as u16, r2 - 1, (c2 - 1) as u16, "", &format_of(&style, None)).map_err(e)?;
            }
        }
        for ((r, c), (input, out)) in cells {
            let (row, col) = (r - 1, (c - 1) as u16);
            let raw = input.and_then(|x| match x {
                J::String(t) => Some(t.clone()),
                _ => x["v"].as_str().map(str::to_string),
            });
            let style = input.map(|x| x["s"].clone()).unwrap_or(J::Null);
            let formula = raw.as_deref().filter(|t| t.starts_with('='));
            // the value shown: a formula's result, a spilled cell, or the value typed
            let value: Option<Result<V, (String, String)>> = match out {
                Some(Out::Ok(v)) => Some(Ok(v.clone())),
                Some(Out::Err { code, msg }) => Some(Err((code.to_string(), msg.clone()))),
                None => raw.as_deref().and_then(|t| super::sheets::classify(i, t)).and_then(|x| match x {
                    super::sheets::Input::Value(v) => Some(Ok(v)),
                    super::sheets::Input::Formula(_) => None,
                }),
            };
            let fmt = format_of(&style, value.as_ref().and_then(|v| v.as_ref().ok()));
            match &value {
                None | Some(Ok(V::None)) => {
                    ws.write_blank(row, col, &fmt).map_err(e)?;
                }
                Some(Err((code, _))) => {
                    ws.write_string_with_format(row, col, code, &fmt).map_err(e)?;
                }
                Some(Ok(v)) => match v {
                    V::Int(n) => {
                        ws.write_number_with_format(row, col, *n as f64, &fmt).map_err(e)?;
                    }
                    V::Dec(d) | V::Pct(d) | V::Money { amt: d, .. } | V::Qty { v: d, .. } | V::Price { amt: d, .. } => {
                        ws.write_number_with_format(row, col, number(d), &fmt).map_err(e)?;
                    }
                    V::Bool(b) => {
                        ws.write_boolean_with_format(row, col, *b, &fmt).map_err(e)?;
                    }
                    V::Date(d) => {
                        ws.write_datetime_with_format(row, col, d, &fmt).map_err(e)?;
                    }
                    V::DateTime(d) => {
                        ws.write_datetime_with_format(row, col, d, &fmt).map_err(e)?;
                    }
                    V::Text(t) => {
                        ws.write_string_with_format(row, col, t.as_ref(), &fmt).map_err(e)?;
                    }
                    other => {
                        let shown = super::sheets::value_json(i, other)["d"].as_str().unwrap_or("").to_string();
                        ws.write_string_with_format(row, col, &shown, &fmt).map_err(e)?;
                    }
                },
            }
            if let Some(f) = formula {
                let mut text = format!("Quadra: {f}");
                if let Some(Err((_, msg))) = &value {
                    text.push_str(&format!("\n{msg}"));
                }
                ws.insert_note(row, col, &Note::new(text).set_author("Quadra")).map_err(e)?;
            }
        }
        for (letters, w) in s["cols_w"].as_object().into_iter().flatten() {
            if let (Some(c), Some(px)) = (super::sheets::col_index(letters), w.as_f64()) {
                ws.set_column_width_pixels((c - 1) as u16, px.clamp(4.0, 2000.0) as u32).map_err(e)?;
            }
        }
        for (row, h) in s["rows_h"].as_object().into_iter().flatten() {
            if let (Ok(r), Some(px)) = (row.parse::<u32>(), h.as_f64()) {
                if r >= 1 {
                    ws.set_row_height_pixels(r - 1, px.clamp(4.0, 2000.0) as u32).map_err(e)?;
                }
            }
        }
        let fr = s["freeze"]["rows"].as_u64().unwrap_or(0) as u32;
        let fc = s["freeze"]["cols"].as_u64().unwrap_or(0) as u16;
        if fr > 0 || fc > 0 {
            ws.set_freeze_panes(fr, fc).map_err(e)?;
        }
    }
    if sheets.is_empty() {
        wb.add_worksheet();
    }
    wb.save_to_buffer().map_err(e)
}
