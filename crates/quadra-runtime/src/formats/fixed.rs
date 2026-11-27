//! Fixed-width lines (CBI, legacy systems, AS/400): `record CbiRow from fixed:`, each field
//! `at "11..20"` (columns from 1, both included; they count characters).
//!
//! The rules, read and written alike:
//!
//! - A line per record, ended by `\r\n` when written (`\n` or `\r\n` when read); empty lines
//!   are skipped. A line shorter than the record reads as if it went on with spaces (editors
//!   drop trailing spaces); the columns no field declares are spaces when written.
//! - Text (and enums, dates, booleans) is left-aligned and padded with spaces; when read, the
//!   spaces after it are not part of it. A text longer than its columns is an error, never
//!   cut.
//! - Numbers are right-aligned and padded with zeros (`-0042`: the sign first). A `Decimal`
//!   says how many of its last digits are decimals, `decimals 2`: `0001250` is 12.50 (no
//!   separator is written). A number that does not fit is an error.
//! - Columns that are only spaces are none for an optional field, the empty text for a
//!   `Text`, an error for the others.
//! - A boolean is `1` / `0` unless `format "S/N"` names them; dates are ISO 8601 unless
//!   `format "%d%m%y"`.
//! - EBCDIC and packed decimals (COMP-3) are not read or written yet: such files are
//!   converted to text first.

use super::{encodable, field_value, fixed_scale, new_record, options, read_scalar, record};
use crate::value::V;
use quadra_ir::formats::{fields, kind, range, Kind};
use quadra_ir::{Field, Model};
use rust_decimal::Decimal;
use std::rc::Rc;

/// The fields of a record with their columns (from 0, the end excluded).
fn layout<'m>(m: &'m Model, rec: &str) -> Result<Vec<(&'m Field, usize, usize)>, String> {
    let e = record(m, rec)?;
    fields(m, e)
        .into_iter()
        .map(|f| {
            let at = f
                .at
                .as_deref()
                .ok_or_else(|| format!("`{rec}.{}` says its columns: `at \"11..20\"`", f.name))?;
            let (a, b) = range(at)?;
            Ok((f, a - 1, b))
        })
        .collect()
}

/// Reads the lines of a fixed-width text into records of `rec`.
pub(super) fn parse_rows(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let fields = layout(m, rec)?;
    let mut out = Vec::new();
    for (n, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut values = Vec::new();
        for (f, a, b) in &fields {
            let Some(ty) = &f.ty else { continue };
            let here = |why: String| {
                format!(
                    "line {}, columns {}..{} (`{}`): {why}",
                    n + 1,
                    a + 1,
                    b,
                    f.name
                )
            };
            let cell: String = (*a..*b)
                .map(|i| chars.get(i).copied().unwrap_or(' '))
                .collect();
            let k = kind(m, ty, "fixed")?;
            let optional = matches!(k, Kind::Optional(_));
            let k = k.unopt();
            let v = if cell.trim().is_empty() {
                match k {
                    _ if optional => V::None,
                    Kind::Text => V::Text(Rc::from("")),
                    _ => return Err(here("missing (only spaces)".into())),
                }
            } else {
                match k {
                    Kind::Text => V::Text(Rc::from(cell.trim_end_matches(' '))),
                    Kind::Decimal => read_decimal(cell.trim(), f.decimals).map_err(here)?,
                    _ => read_scalar(m, k, cell.trim(), f, '.').map_err(here)?,
                }
            };
            values.push((f.name.clone(), v));
        }
        out.push(new_record(m, rec, values));
    }
    Ok(V::List(Rc::new(out)))
}

/// A number whose last `decimals` digits are its decimals (`-0001250`, 2: -12.50).
fn read_decimal(t: &str, decimals: Option<u32>) -> Result<V, String> {
    let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
    let Some(n) = decimals else {
        return t
            .parse::<Decimal>()
            .map(V::Dec)
            .map_err(|_| format!("a number was expected, not `{t}`"));
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "digits were expected, not `{t}` (the last {n} are the decimals, without a separator)"
        ));
    }
    let i: i128 = digits
        .parse()
        .map_err(|_| format!("`{t}` is too large a number"))?;
    let i = if t.starts_with('-') { -i } else { i };
    Decimal::try_from_i128_with_scale(i, n)
        .map(V::Dec)
        .map_err(|_| format!("`{t}` is too large a number"))
}

/// Writes records of `rec` as fixed-width lines, each ended by `\r\n`.
pub(super) fn render_rows(m: &Model, rec: &str, rows: &[V]) -> Result<String, String> {
    let fields = layout(m, rec)?;
    let enc = options(record(m, rec)?)?.encoding;
    let width = fields.iter().map(|(_, _, b)| *b).max().unwrap_or(0);
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let V::Rec { fields: values, .. } = r else {
            continue;
        };
        let mut line = vec![' '; width];
        for (f, a, b) in &fields {
            let Some(ty) = &f.ty else { continue };
            let here = |why: String| {
                format!(
                    "row {}, columns {}..{} (`{}`): {why}",
                    i + 1,
                    a + 1,
                    b,
                    f.name
                )
            };
            let k = kind(m, ty, "fixed")?;
            let v = field_value(values, f);
            if matches!(v, V::None) && matches!(k, Kind::Optional(_)) {
                continue;
            }
            let cell = write_cell(k.unopt(), &v, f, b - a).map_err(here)?;
            encodable(&cell, enc).map_err(here)?;
            for (j, c) in cell.chars().enumerate() {
                line[a + j] = c;
            }
        }
        out.extend(line);
        out.push_str("\r\n");
    }
    Ok(out)
}

/// A value in `width` columns: numbers right-aligned with zeros, the rest left-aligned with
/// spaces; what does not fit is an error.
fn write_cell(k: &Kind, v: &V, f: &Field, width: usize) -> Result<String, String> {
    let number = match (k, v) {
        (Kind::Int, V::Int(i)) => Some((*i < 0, i.unsigned_abs().to_string())),
        (Kind::Decimal, V::Dec(d) | V::Pct(d)) => Some(decimal_digits(*d, f.decimals)?),
        (Kind::Decimal, V::Int(i)) => Some(decimal_digits(Decimal::from(*i), f.decimals)?),
        _ => None,
    };
    if let Some((negative, digits)) = number {
        let sign = usize::from(negative);
        if digits.len() + sign > width {
            return Err(format!(
                "{}{digits} does not fit in {width} columns",
                if negative { "-" } else { "" }
            ));
        }
        return Ok(format!(
            "{}{}{digits}",
            if negative { "-" } else { "" },
            "0".repeat(width - sign - digits.len())
        ));
    }
    let text = match (k, v) {
        (Kind::Bool, V::Bool(b)) if f.pattern.is_none() => (if *b { "1" } else { "0" }).into(),
        _ => super::write_scalar(k, v, f, '.')?,
    };
    if text.contains(['\n', '\r']) {
        return Err("a new line cannot be in a fixed-width line".into());
    }
    let len = text.chars().count();
    if len > width {
        return Err(format!(
            "`{text}` is {len} characters, the columns hold {width}"
        ));
    }
    Ok(format!("{text}{}", " ".repeat(width - len)))
}

/// The sign and the digits of a number whose last `decimals` digits are its decimals.
fn decimal_digits(d: Decimal, decimals: Option<u32>) -> Result<(bool, String), String> {
    let d = match decimals {
        Some(n) => fixed_scale(d, n)?,
        None => d.normalize(),
    };
    let digits = if decimals.is_some() {
        d.mantissa().unsigned_abs().to_string()
    } else {
        d.abs().to_string()
    };
    Ok((d.is_sign_negative() && !d.is_zero(), digits))
}
