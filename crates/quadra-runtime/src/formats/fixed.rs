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
//! - In an EBCDIC encoding (`encoding="ibm-280"`), the file is records of bytes back to back,
//!   without new lines (a mainframe's fixed records, as a binary transfer gives them), each
//!   `length` bytes (else up to the end of its last field); columns count bytes. A number
//!   may be `packed` (COMP-3: two digits a byte, the sign in the last half-byte) or `zoned`
//!   (a digit a byte, the sign in the zone of the last one), with its `decimals`. Read as
//!   text (`parse_rows(text)`, `rows.fixed()`), each byte is the character of its code page,
//!   so that the bytes of packed numbers are kept as they are.

use super::ebcdic;
use super::{encodable, field_value, fixed_scale, new_record, options, read_scalar, record};
use crate::value::V;
use quadra_ir::formats::{fields, kind, range, Kind};
use quadra_ir::{Field, Model};
use rust_decimal::Decimal;
use std::rc::Rc;

/// The fields of a record with their kind and their columns (from 0, the end excluded).
type Layout<'m> = Vec<(&'m Field, Kind, usize, usize)>;

fn layout<'m>(m: &'m Model, rec: &str) -> Result<Layout<'m>, String> {
    let e = record(m, rec)?;
    fields(m, e)
        .into_iter()
        .filter_map(|f| f.ty.as_ref().map(|ty| (f, ty)))
        .map(|(f, ty)| {
            let at = f
                .at
                .as_deref()
                .ok_or_else(|| format!("`{rec}.{}` says its columns: `at \"11..20\"`", f.name))?;
            let (a, b) = range(at)?;
            Ok((f, kind(m, ty, "fixed")?, a - 1, b))
        })
        .collect()
}

/// How a number is held in its bytes: as digits (text), packed (COMP-3) or zoned.
#[derive(Clone, Copy, PartialEq)]
enum Digits {
    Text,
    Packed,
    Zoned,
}

fn digits(f: &Field) -> Digits {
    match f.pattern.as_deref() {
        Some("packed") => Digits::Packed,
        Some("zoned") => Digits::Zoned,
        _ => Digits::Text,
    }
}

/// The length of a record: `length`, else the end of its last field.
fn width(layout: &Layout, length: Option<usize>) -> usize {
    let last = layout.iter().map(|(_, _, _, b)| *b).max().unwrap_or(0);
    length.unwrap_or(last).max(last)
}

/// Reads the lines of a fixed-width text into records of `rec`.
pub(super) fn parse_rows(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let mut out = Vec::new();
    for (_, row) in parse_each(m, rec, text)? {
        out.push(row?);
    }
    Ok(V::List(Rc::new(out)))
}

/// Reads each line of a fixed-width text on its own: its number, and its record or what is
/// wrong with it. In EBCDIC, the text is the characters of the bytes (`rows.fixed()`).
pub(super) fn parse_each(m: &Model, rec: &str, text: &str) -> Result<super::Rows, String> {
    let opts = options(record(m, rec)?)?;
    if let Some(page) = ebcdic::page(opts.encoding) {
        let bytes = ebcdic::encode(page, text).map_err(|c| {
            format!(
                "`{c}` is not a character of {}: the text of a file in EBCDIC is that of its bytes",
                opts.encoding.name()
            )
        })?;
        return parse_each_bytes(m, rec, &bytes);
    }
    let fields = layout(m, rec)?;
    let mut out = Vec::new();
    for (n, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let row = (|| -> Result<V, String> {
            let mut values = Vec::with_capacity(fields.len());
            for (f, k, a, b) in &fields {
                let here = |why: String| {
                    format!(
                        "line {}, columns {}..{} (`{}`): {why}",
                        n + 1,
                        a + 1,
                        b,
                        f.name
                    )
                };
                if digits(f) != Digits::Text {
                    return Err(here(
                        "packed and zoned numbers are in files in EBCDIC (`encoding=\"ibm-280\"`)"
                            .into(),
                    ));
                }
                let cell: String = (*a..*b)
                    .map(|i| chars.get(i).copied().unwrap_or(' '))
                    .collect();
                values.push((f.name.clone(), read_cell(m, f, k, &cell).map_err(here)?));
            }
            Ok(new_record(m, rec, values))
        })();
        out.push((n + 1, row));
    }
    Ok(out)
}

/// Reads the records of a file in EBCDIC: `length` bytes each, back to back.
pub(super) fn parse_each_bytes(m: &Model, rec: &str, bytes: &[u8]) -> Result<super::Rows, String> {
    let opts = options(record(m, rec)?)?;
    let Some(page) = ebcdic::page(opts.encoding) else {
        return parse_each(m, rec, &super::decode(bytes, opts.encoding)?);
    };
    let fields = layout(m, rec)?;
    let len = width(&fields, opts.length);
    if len == 0 {
        return Ok(Vec::new());
    }
    if !bytes.len().is_multiple_of(len) {
        return Err(format!(
            "the file is {} bytes, not a whole number of records of {len} bytes (records in EBCDIC are back to back, without new lines; `length=…` says their length)",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / len);
    for (n, r) in bytes.chunks(len).enumerate() {
        let row = (|| -> Result<V, String> {
            let mut values = Vec::with_capacity(fields.len());
            for (f, k, a, b) in &fields {
                let here = |why: String| {
                    format!(
                        "record {}, bytes {}..{} (`{}`): {why}",
                        n + 1,
                        a + 1,
                        b,
                        f.name
                    )
                };
                let cell = &r[*a..*b];
                let v = match digits(f) {
                    Digits::Text => read_cell(m, f, k, &ebcdic::decode(page, cell)),
                    d => read_number(f, k, cell, d),
                };
                values.push((f.name.clone(), v.map_err(here)?));
            }
            Ok(new_record(m, rec, values))
        })();
        out.push((n + 1, row));
    }
    Ok(out)
}

/// A value from the text of its columns.
fn read_cell(m: &Model, f: &Field, k: &Kind, cell: &str) -> Result<V, String> {
    let optional = matches!(k, Kind::Optional(_));
    let k = k.unopt();
    Ok(if cell.trim().is_empty() {
        match k {
            _ if optional => V::None,
            Kind::Text => V::Text(Rc::from("")),
            _ => return Err("missing (only spaces)".into()),
        }
    } else {
        match k {
            Kind::Text => V::Text(Rc::from(cell.trim_end_matches(' '))),
            Kind::Decimal => read_decimal(cell.trim(), f.decimals)?,
            _ => read_scalar(m, k, cell.trim(), f, '.')?,
        }
    })
}

/// A packed or zoned number from its bytes; spaces (or zeros: low values) are none.
fn read_number(f: &Field, k: &Kind, cell: &[u8], d: Digits) -> Result<V, String> {
    if cell.iter().all(|b| *b == 0x40) || cell.iter().all(|b| *b == 0) {
        return if matches!(k, Kind::Optional(_)) {
            Ok(V::None)
        } else {
            Err("missing (only spaces)".into())
        };
    }
    let n = match d {
        Digits::Packed => ebcdic::unpack(cell)?,
        _ => ebcdic::unzone(cell)?,
    };
    match k.unopt() {
        Kind::Int => i64::try_from(n)
            .map(V::Int)
            .map_err(|_| format!("{n} is too large an integer")),
        _ => Decimal::try_from_i128_with_scale(n, f.decimals.unwrap_or(0))
            .map(V::Dec)
            .map_err(|_| format!("{n} is too large a number")),
    }
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

/// Writes records of `rec` as fixed-width lines, each ended by `\r\n`; in EBCDIC, the
/// characters of the bytes of its records.
pub(super) fn render_rows(m: &Model, rec: &str, rows: &[V]) -> Result<String, String> {
    let opts = options(record(m, rec)?)?;
    if let Some(page) = ebcdic::page(opts.encoding) {
        return Ok(ebcdic::decode(page, &render_bytes(m, rec, rows)?));
    }
    let fields = layout(m, rec)?;
    let enc = opts.encoding;
    let width = width(&fields, opts.length);
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let V::Rec { fields: values, .. } = r else {
            continue;
        };
        let mut line = vec![' '; width];
        for (f, k, a, b) in &fields {
            let here = |why: String| {
                format!(
                    "row {}, columns {}..{} (`{}`): {why}",
                    i + 1,
                    a + 1,
                    b,
                    f.name
                )
            };
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

/// Writes records of `rec` as the bytes of their file: in EBCDIC, records back to back.
pub(super) fn render_bytes(m: &Model, rec: &str, rows: &[V]) -> Result<Vec<u8>, String> {
    let opts = options(record(m, rec)?)?;
    let Some(page) = ebcdic::page(opts.encoding) else {
        return super::encode(&render_rows(m, rec, rows)?, opts.encoding);
    };
    let fields = layout(m, rec)?;
    let width = width(&fields, opts.length);
    let mut out = Vec::with_capacity(width * rows.len());
    for (i, r) in rows.iter().enumerate() {
        let V::Rec { fields: values, .. } = r else {
            continue;
        };
        let mut record = vec![0x40u8; width];
        for (f, k, a, b) in &fields {
            let here = |why: String| {
                format!(
                    "row {}, bytes {}..{} (`{}`): {why}",
                    i + 1,
                    a + 1,
                    b,
                    f.name
                )
            };
            let v = field_value(values, f);
            if matches!(v, V::None) && matches!(k, Kind::Optional(_)) {
                continue;
            }
            let bytes = match digits(f) {
                Digits::Text => {
                    let cell = write_cell(k.unopt(), &v, f, b - a).map_err(here)?;
                    ebcdic::encode(page, &cell).map_err(|c| {
                        here(format!(
                            "`{c}` cannot be written in {}",
                            opts.encoding.name()
                        ))
                    })?
                }
                d => {
                    let n = integer(&v, f).map_err(here)?;
                    match d {
                        Digits::Packed => ebcdic::pack(n, b - a),
                        _ => ebcdic::zone(n, b - a),
                    }
                    .map_err(here)?
                }
            };
            record[*a..*b].copy_from_slice(&bytes);
        }
        out.extend(record);
    }
    Ok(out)
}

/// The digits of a number as an integer: a `Decimal` with its `decimals` (`12.50`, 2: 1250).
fn integer(v: &V, f: &Field) -> Result<i128, String> {
    match v {
        V::Int(i) if f.decimals.is_none() => Ok(i128::from(*i)),
        V::Int(i) => integer(&V::Dec(Decimal::from(*i)), f),
        V::Dec(d) | V::Pct(d) => Ok(fixed_scale(*d, f.decimals.unwrap_or(0))?.mantissa()),
        V::None => Err("missing".into()),
        other => Err(format!("{other} is not a number")),
    }
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
