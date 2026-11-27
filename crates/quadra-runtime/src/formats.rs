//! Records read from and written to an external format: `record ShopOrder from json:`,
//! `record Invoice from xml:`, `record PosRow from csv(sep=";"):`, `record CbiRow from fixed:`.
//!
//! A document (JSON, XML) is one record: `ShopOrder.parse(text)` reads it, `order.json()` and
//! `invoice.xml()` write it. A file of rows (CSV, fixed-width) is a list of records:
//! `PosRow.parse_rows(text)` reads every row, `rows.csv()` and `rows.fixed()` write the file.
//! All of them are pure: the same text gives the same records and the same records the same
//! text, so they can be used in functions, in tests and to build what an `outbound` sends.
//!
//! Where a field is in each format is in `quadra_ir::formats`; how each format reads and
//! writes is in its module (`xml`, `csv`, `fixed`; JSON below). The types a field may have:
//!
//! | Type | JSON | XML, CSV, fixed-width (text) |
//! |---|---|---|
//! | `Text`, `Code`, `Email`, `Phone`, `VatNumber`, `Barcode` | a string (a number is read as its text, for ids too large for a number) | the text |
//! | `Int` | an integer (or a string of one) | `-42` |
//! | `Decimal` | a number, written exactly; read from a number or a string | `12.50` (CSV: with its `decimal` separator; fixed: the last `decimals` digits) |
//! | `Bool` | `true` / `false` | `true` / `false` (read: also `1` / `0`; fixed: `1` / `0`); `format "S/N"` names them |
//! | `Date`, `DateTime` | `"2026-11-27"`, `"2026-11-27T10:14:03Z"` (an instant without a zone is read as UTC) | the same, or `format "%d/%m/%Y"` |
//! | an enum | the name of the variant | the name of the variant |
//! | another record of the format | an object | XML: an element |
//! | `[T]` | an array | XML: repeated elements |
//! | `T?` | missing or `null` when none | missing (XML) or empty (CSV, fixed) when none |
//!
//! `format "…"` (a date's pattern, or the text of true and false) and `decimals N` (the
//! decimals a number is written with) apply in every format.

mod csv;
mod fixed;
#[cfg(test)]
mod tests;
mod xml;

use crate::value::V;
use chrono::{NaiveDate, NaiveDateTime};
use quadra_ir::{Entity, Field, Model};
use rust_decimal::Decimal;
use serde_json::Value as J;
use std::rc::Rc;
use std::str::FromStr;

pub use quadra_ir::formats::{kind, place, Encoding, Kind, FORMATS};
pub use xml::{MAX_BYTES as XML_MAX_BYTES, MAX_DEPTH as XML_MAX_DEPTH};

fn record<'m>(m: &'m Model, name: &str) -> Result<&'m Entity, String> {
    m.entity(name)
        .filter(|e| e.format.is_some())
        .ok_or_else(|| format!("`{name}` is not a record `from` a format"))
}

/// The format of a record.
fn format_of<'m>(m: &'m Model, rec: &str) -> Result<&'m str, String> {
    Ok(record(m, rec)?.format.as_deref().unwrap_or("json"))
}

/// The options of a record's format (the checker refused wrong ones).
fn options(e: &Entity) -> Result<quadra_ir::formats::Options, String> {
    quadra_ir::formats::options(e).map_err(|(_, why)| why)
}

/// A field's value in a record, none when it is not set.
fn field_value(fields: &[(String, V)], f: &Field) -> V {
    fields
        .iter()
        .find(|(n, _)| n == &f.name)
        .map(|(_, v)| v.clone())
        .unwrap_or(V::None)
}

/// A record read: its fields in the order of the declaration, as a constructor makes them
/// (the formulas none, computed when read), so that it equals the same record built in code.
fn new_record(m: &Model, rec: &str, mut read: Vec<(String, V)>) -> V {
    let mut fields = Vec::with_capacity(read.len());
    if let Some(e) = m.entity(rec) {
        for f in m.all_fields(e) {
            let v = match read.iter().position(|(n, _)| n == &f.name) {
                Some(i) => read.swap_remove(i).1,
                None => V::None,
            };
            fields.push((f.name.clone(), v));
        }
    }
    fields.extend(read);
    V::Rec {
        ent: Rc::from(rec),
        fields: Rc::new(fields),
    }
}

// ---- documents and files, whatever the format --------------------------------------------

/// Reads a document (a record `from json` or `from xml`).
pub fn parse(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    match format_of(m, rec)? {
        "xml" => xml::parse(m, rec, text),
        "json" => parse_json(m, rec, text),
        f => Err(format!(
            "{} is read row by row: `{rec}.parse_rows(text)`",
            quadra_ir::formats::title(f)
        )),
    }
}

/// Writes a document (a record `from json` or `from xml`), with the content type it is sent
/// with.
pub fn render(m: &Model, v: &V) -> Result<(String, &'static str), String> {
    let V::Rec { ent, .. } = v else {
        return Err(format!("{v} is not a record `from` a format"));
    };
    match format_of(m, ent)? {
        "xml" => Ok((xml::render(m, v)?, "application/xml")),
        "json" => Ok((render_json(m, v)?, "application/json")),
        f => Err(format!(
            "a record `from {f}` is a row: the file is written from the list of its rows"
        )),
    }
}

/// Reads every row of a file (a record `from csv` or `from fixed`) as a list of records.
pub fn parse_rows(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format_of(m, rec)? {
        "csv" => csv::parse_rows(m, rec, text),
        "fixed" => fixed::parse_rows(m, rec, text),
        _ => Err(format!("`{rec}` is one document: `{rec}.parse(text)`")),
    }
}

/// Writes rows of a record `from csv` or `from fixed` as a file (the header included).
pub fn render_rows(m: &Model, rec: &str, rows: &[V]) -> Result<String, String> {
    for (i, r) in rows.iter().enumerate() {
        match r {
            V::Rec { ent, .. } if &**ent == rec => {}
            other => return Err(format!("row {}: {other} is not a `{rec}`", i + 1)),
        }
    }
    match format_of(m, rec)? {
        "csv" => csv::render_rows(m, rec, rows),
        "fixed" => fixed::render_rows(m, rec, rows),
        _ => Err(format!("`{rec}` is one document, not a row of a file")),
    }
}

/// Reads the bytes of a file of rows: decoded with the record's `encoding`, then
/// `parse_rows`. For the hosts that receive files as bytes.
pub fn parse_rows_bytes(m: &Model, rec: &str, bytes: &[u8]) -> Result<V, String> {
    let enc = options(record(m, rec)?)?.encoding;
    parse_rows(m, rec, &decode(bytes, enc)?)
}

/// Writes rows as the bytes of the file, in the record's `encoding`.
pub fn render_rows_bytes(m: &Model, rec: &str, rows: &[V]) -> Result<Vec<u8>, String> {
    let enc = options(record(m, rec)?)?.encoding;
    encode(&render_rows(m, rec, rows)?, enc)
}

/// The text of a file's bytes. UTF-8 must be valid (a byte that is not says its line);
/// latin1 (ISO-8859-1) is one character per byte.
pub fn decode(bytes: &[u8], enc: Encoding) -> Result<String, String> {
    match enc {
        Encoding::Utf8 => String::from_utf8(bytes.to_vec()).map_err(|e| {
            let at = e.utf8_error().valid_up_to();
            let line = bytes[..at].iter().filter(|b| **b == b'\n').count() + 1;
            format!("line {line}: not UTF-8 (a file in latin1 says `encoding=\"latin1\"`)")
        }),
        Encoding::Latin1 => Ok(bytes.iter().map(|b| *b as char).collect()),
    }
}

/// The bytes of a text in an encoding: latin1 refuses a character after U+00FF.
pub fn encode(text: &str, enc: Encoding) -> Result<Vec<u8>, String> {
    match enc {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    out.push(b'\n');
                }
                for c in line.chars() {
                    match u8::try_from(u32::from(c)) {
                        Ok(b) => out.push(b),
                        Err(_) => {
                            return Err(format!(
                                "line {}: `{c}` cannot be written in latin1 (ISO-8859-1)",
                                i + 1
                            ))
                        }
                    }
                }
            }
            Ok(out)
        }
    }
}

/// A cell that the encoding of its file cannot hold.
fn encodable(s: &str, enc: Encoding) -> Result<(), String> {
    match (enc, s.chars().find(|c| u32::from(*c) > 0xFF)) {
        (Encoding::Latin1, Some(c)) => {
            Err(format!("`{c}` cannot be written in latin1 (ISO-8859-1)"))
        }
        _ => Ok(()),
    }
}

// ---- values as text (XML, CSV, fixed-width) ----------------------------------------------

/// Reads one value written as text (not a list, an optional or a record): `decimal` is the
/// decimal separator. The error says what was expected and what was found.
fn read_scalar(m: &Model, k: &Kind, s: &str, f: &Field, decimal: char) -> Result<V, String> {
    if let Kind::Text = k {
        return Ok(V::Text(Rc::from(s)));
    }
    let t = s.trim();
    let bad = |what: &str| Err(format!("{what} was expected, not `{t}`"));
    match k {
        Kind::Int => match t.strip_prefix('+').unwrap_or(t).parse::<i64>() {
            Ok(i) => Ok(V::Int(i)),
            Err(_) => bad("an integer"),
        },
        Kind::Decimal => {
            let normal = if decimal == '.' {
                t.to_string()
            } else if t.contains('.') {
                return Err(format!(
                    "a number with `{decimal}` before the decimals was expected, not `{t}`"
                ));
            } else {
                t.replace(decimal, ".")
            };
            match Decimal::from_str(normal.strip_prefix('+').unwrap_or(&normal)) {
                Ok(d) => Ok(V::Dec(d)),
                Err(_) => bad("a number"),
            }
        }
        Kind::Bool => match &f.pattern {
            Some(p) => {
                let (yes, no) = p.split_once('/').unwrap_or((p, ""));
                if t.eq_ignore_ascii_case(yes) {
                    Ok(V::Bool(true))
                } else if t.eq_ignore_ascii_case(no) {
                    Ok(V::Bool(false))
                } else {
                    bad(&format!("`{yes}` or `{no}`"))
                }
            }
            None => match t.to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(V::Bool(true)),
                "false" | "0" => Ok(V::Bool(false)),
                _ => bad("true or false"),
            },
        },
        Kind::Date => {
            let p = f.pattern.as_deref().unwrap_or("%Y-%m-%d");
            match NaiveDate::parse_from_str(t, p) {
                Ok(d) => Ok(V::Date(d)),
                Err(_) => bad(&format!("a date ({})", example_date(p))),
            }
        }
        Kind::DateTime => {
            let v = match &f.pattern {
                Some(p) => NaiveDateTime::parse_from_str(t, p).ok(),
                None => datetime(t),
            };
            match v {
                Some(v) => Ok(V::DateTime(v)),
                None => bad(&format!(
                    "an instant ({})",
                    f.pattern
                        .as_deref()
                        .map_or("2026-11-27T10:14:03Z".to_string(), example_datetime)
                )),
            }
        }
        Kind::Enum(name) => variant(m, name, t),
        Kind::Text | Kind::Record(_) | Kind::List(_) | Kind::Optional(_) => {
            Err("one value was expected".into())
        }
    }
}

/// A variant of an enum by its name.
fn variant(m: &Model, name: &str, s: &str) -> Result<V, String> {
    let e = m.enum_def(name);
    if !e.is_some_and(|e| e.variants.iter().any(|v| v.name == s)) {
        let known: Vec<&str> = e
            .map(|e| e.variants.iter().map(|v| v.name.as_str()).collect())
            .unwrap_or_default();
        return Err(format!(
            "`{s}` is not a value of {name} ({})",
            known.join(", ")
        ));
    }
    Ok(V::Variant {
        name: Rc::from(s),
        fields: Rc::new(Vec::new()),
    })
}

fn example_date(p: &str) -> String {
    NaiveDate::from_ymd_opt(2026, 11, 27)
        .map(|d| d.format(p).to_string())
        .unwrap_or_default()
}

fn example_datetime(p: &str) -> String {
    NaiveDate::from_ymd_opt(2026, 11, 27)
        .and_then(|d| d.and_hms_opt(10, 14, 3))
        .map(|d| d.format(p).to_string())
        .unwrap_or_default()
}

/// Writes one value as text (not a list, an optional or a record).
fn write_scalar(k: &Kind, v: &V, f: &Field, decimal: char) -> Result<String, String> {
    Ok(match (k, v) {
        (Kind::Decimal, V::Dec(d) | V::Pct(d)) => decimal_text(*d, f.decimals, decimal)?,
        (Kind::Decimal, V::Int(i)) => decimal_text(Decimal::from(*i), f.decimals, decimal)?,
        (Kind::Bool, V::Bool(b)) => match &f.pattern {
            Some(p) => {
                let (yes, no) = p.split_once('/').unwrap_or((p, ""));
                if *b { yes } else { no }.to_string()
            }
            None => b.to_string(),
        },
        (Kind::Date, V::Date(d)) => d
            .format(f.pattern.as_deref().unwrap_or("%Y-%m-%d"))
            .to_string(),
        (Kind::DateTime, V::DateTime(t)) => t
            .format(f.pattern.as_deref().unwrap_or("%Y-%m-%dT%H:%M:%SZ"))
            .to_string(),
        (_, V::Text(t)) => t.to_string(),
        (_, V::Variant { name, .. }) => name.to_string(),
        (_, V::None) => return Err("missing".into()),
        (_, other) => other.to_string(),
    })
}

/// A number as text: with `decimals`, exactly that many (a number with more is refused, not
/// rounded); `decimal` is the separator.
fn decimal_text(d: Decimal, decimals: Option<u32>, decimal: char) -> Result<String, String> {
    let s = match decimals {
        None => d.normalize().to_string(),
        Some(n) => fixed_scale(d, n)?.to_string(),
    };
    Ok(if decimal == '.' {
        s
    } else {
        s.replace('.', &decimal.to_string())
    })
}

/// The number with exactly `n` decimals, refused when it has more.
fn fixed_scale(d: Decimal, n: u32) -> Result<Decimal, String> {
    if d.normalize().scale() > n {
        return Err(format!(
            "{} has more than {n} decimals (round it first)",
            d.normalize()
        ));
    }
    let mut x = d;
    x.rescale(n);
    Ok(x)
}

// ---- JSON ----------------------------------------------------------------------------------

/// Reads a JSON document into a record of `rec`.
/// A message received as the record `rec`: a document (`from json`, `from xml`) is one
/// record; a file of rows (`from csv`, `from fixed`) is the list of its rows, read from its
/// bytes in the record's encoding.
pub fn read_message(m: &Model, rec: &str, body: &[u8]) -> Result<V, String> {
    let rows = m
        .entity(rec)
        .is_some_and(|e| matches!(e.format.as_deref(), Some("csv" | "fixed")));
    if rows {
        return parse_rows_bytes(m, rec, body);
    }
    let text =
        std::str::from_utf8(body).map_err(|_| "the message is not text (UTF-8)".to_string())?;
    parse(m, rec, text)
}

pub fn parse_json(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let doc: J = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    read_record(m, rec, &doc, rec)
}

fn read_record(m: &Model, rec: &str, doc: &J, at: &str) -> Result<V, String> {
    let e = record(m, rec)?;
    if !doc.is_object() {
        return Err(format!("{at}: an object was expected"));
    }
    let mut fields = Vec::new();
    for f in quadra_ir::formats::fields(m, e) {
        let Some(ty) = &f.ty else { continue };
        let keys = place(f);
        let mut cur = Some(doc);
        for k in &keys {
            cur = cur.and_then(|c| c.get(k));
        }
        let k = kind(m, ty, "json")?;
        let here = format!("{at}.{}", keys.join("."));
        let v = read(m, &k, f, cur.unwrap_or(&J::Null), &here)?;
        fields.push((f.name.clone(), v));
    }
    Ok(new_record(m, rec, fields))
}

fn read(m: &Model, k: &Kind, f: &Field, j: &J, at: &str) -> Result<V, String> {
    if let Kind::Optional(inner) = k {
        return if j.is_null() {
            Ok(V::None)
        } else {
            read(m, inner, f, j, at)
        };
    }
    if j.is_null() {
        return Err(format!("{at}: missing"));
    }
    let bad = |what: &str| Err(format!("{at}: {what} was expected, not {j}"));
    // a date or a boolean written with a `format` is a string of it
    if f.pattern.is_some() && matches!(k, Kind::Date | Kind::DateTime | Kind::Bool) {
        return match j.as_str() {
            Some(s) => read_scalar(m, k, s, f, '.').map_err(|e| format!("{at}: {e}")),
            None => bad("text"),
        };
    }
    match k {
        Kind::Text => match j {
            J::String(s) => Ok(V::Text(Rc::from(s.as_str()))),
            J::Number(n) => Ok(V::Text(Rc::from(n.to_string().as_str()))),
            _ => bad("text"),
        },
        Kind::Int => match j {
            J::Number(n) if n.as_i64().is_some() => Ok(V::Int(n.as_i64().unwrap())),
            J::String(s) if s.trim().parse::<i64>().is_ok() => {
                Ok(V::Int(s.trim().parse().unwrap()))
            }
            _ => bad("an integer"),
        },
        Kind::Decimal => {
            let text = match j {
                J::Number(n) => n.to_string(),
                J::String(s) => s.trim().to_string(),
                _ => return bad("a number"),
            };
            Decimal::from_str(&text)
                .or_else(|_| Decimal::from_scientific(&text))
                .map(V::Dec)
                .or_else(|_| bad("a number"))
        }
        Kind::Bool => match j {
            J::Bool(b) => Ok(V::Bool(*b)),
            _ => bad("true or false"),
        },
        Kind::Date => match j
            .as_str()
            .and_then(|s| NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok())
        {
            Some(d) => Ok(V::Date(d)),
            None => bad("a date (2026-11-27)"),
        },
        Kind::DateTime => match j.as_str().and_then(datetime) {
            Some(t) => Ok(V::DateTime(t)),
            None => bad("an instant (2026-11-27T10:14:03Z)"),
        },
        Kind::Enum(name) => {
            let Some(s) = j.as_str() else {
                return bad("text");
            };
            variant(m, name, s).map_err(|e| format!("{at}: {e}"))
        }
        Kind::Record(r) => read_record(m, r, j, at),
        Kind::List(inner) => match j {
            J::Array(a) => Ok(V::List(Rc::new(
                a.iter()
                    .enumerate()
                    .map(|(i, x)| read(m, inner, f, x, &format!("{at}[{i}]")))
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            _ => bad("a list"),
        },
        Kind::Optional(_) => unreachable!(),
    }
}

/// An instant as text: RFC 3339 with its zone, or without one (UTC).
fn datetime(s: &str) -> Option<NaiveDateTime> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.naive_utc())
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok())
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
}

/// Writes a record of a `from json` record type as a JSON document.
pub fn render_json(m: &Model, v: &V) -> Result<String, String> {
    let mut out = String::new();
    write_value(m, None, None, v, &mut out)?;
    Ok(out)
}

fn write_value(
    m: &Model,
    k: Option<&Kind>,
    f: Option<&Field>,
    v: &V,
    out: &mut String,
) -> Result<(), String> {
    let s = |out: &mut String, t: &str| out.push_str(&serde_json::to_string(t).unwrap_or_default());
    let pattern = f.and_then(|f| f.pattern.as_ref());
    match v {
        V::None => out.push_str("null"),
        V::Bool(b) => match pattern {
            // `format "S/N"`: the text of true and of false
            Some(p) => {
                let (yes, no) = p.split_once('/').unwrap_or((p, ""));
                s(out, if *b { yes } else { no })
            }
            None => out.push_str(if *b { "true" } else { "false" }),
        },
        V::Int(i) => {
            if matches!(k, Some(Kind::Text)) {
                s(out, &i.to_string())
            } else {
                out.push_str(&i.to_string())
            }
        }
        V::Dec(d) | V::Pct(d) => match f.and_then(|f| f.decimals) {
            Some(n) => out.push_str(&fixed_scale(*d, n)?.to_string()),
            None => out.push_str(&d.normalize().to_string()),
        },
        V::Text(t) => s(out, t),
        V::Date(d) => s(
            out,
            &d.format(pattern.map_or("%Y-%m-%d", |p| p.as_str()))
                .to_string(),
        ),
        V::DateTime(t) => s(
            out,
            &t.format(pattern.map_or("%Y-%m-%dT%H:%M:%SZ", |p| p.as_str()))
                .to_string(),
        ),
        V::Variant { name, .. } => s(out, name),
        V::List(items) | V::Set(items) => {
            let inner = match k {
                Some(Kind::List(i)) => Some(i.as_ref().clone()),
                _ => None,
            };
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(m, inner.as_ref(), f, x, out)?;
            }
            out.push(']');
        }
        V::Rec { ent, fields } => {
            let e = record(m, ent)?;
            // nested objects for the `at` paths, in the order of the fields
            let mut tree = Tree::default();
            for f in quadra_ir::formats::fields(m, e) {
                let Some(ty) = &f.ty else { continue };
                let value = field_value(fields, f);
                let k = kind(m, ty, "json")?;
                // an optional without a value is left out
                if matches!(value, V::None) && matches!(k, Kind::Optional(_)) {
                    continue;
                }
                let k = match k {
                    Kind::Optional(i) => *i,
                    k => k,
                };
                let mut text = String::new();
                write_value(m, Some(&k), Some(f), &value, &mut text)
                    .map_err(|e| format!("{ent}.{}: {e}", f.name))?;
                tree.put(&place(f), text);
            }
            tree.write(out);
        }
        other => return Err(format!("{other} cannot be written to JSON")),
    }
    Ok(())
}

/// The keys of an object being written, each a value already written or a nested object.
#[derive(Default)]
struct Tree(Vec<(String, Node)>);

enum Node {
    Value(String),
    Object(Tree),
}

impl Tree {
    fn put(&mut self, keys: &[String], value: String) {
        let Some((first, rest)) = keys.split_first() else {
            return;
        };
        if rest.is_empty() {
            self.0.push((first.clone(), Node::Value(value)));
            return;
        }
        if let Some((_, Node::Object(t))) = self.0.iter_mut().find(|(k, _)| k == first) {
            t.put(rest, value);
            return;
        }
        let mut t = Tree::default();
        t.put(rest, value);
        self.0.push((first.clone(), Node::Object(t)));
    }

    fn write(&self, out: &mut String) {
        out.push('{');
        for (i, (k, n)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(k).unwrap_or_default());
            out.push(':');
            match n {
                Node::Value(v) => out.push_str(v),
                Node::Object(t) => t.write(out),
            }
        }
        out.push('}');
    }
}
