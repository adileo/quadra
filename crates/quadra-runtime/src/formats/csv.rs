//! CSV: `record PosRow from csv(sep=";", decimal=",", header=true, encoding="latin1"):`.
//!
//! The rules, read and written alike:
//!
//! - Cells are separated by `sep` and rows by a new line (`\r\n` when written; `\n` or `\r\n`
//!   when read). Quoting is RFC 4180's: a cell that holds the separator, a quote or a new line
//!   is between quotes, and a quote inside is doubled (`"say ""hi"""`). A quote in the middle
//!   of a cell that does not start with one is kept as written. Empty lines are skipped.
//! - With `header=true` (the default) the first row names the columns: a field is the column
//!   of its name, or of `at "NAME"`; the columns the record does not declare are ignored, and
//!   a column missing from the header is an error (none for an optional field). With
//!   `header=false`, `at "3"` is the position (from 1), else the order of the field.
//! - An empty cell is none for an optional field, the empty text for a `Text`, and an error
//!   for the others. A row too short for a field is an error, with its line and column.
//! - Numbers use the `decimal` separator; dates and instants `format` or ISO 8601.
//! - Formulas: a text cell that starts with `=`, `+`, `-`, `@` (or a tab, or a carriage
//!   return) is written after a `'`, so that a spreadsheet does not run it; when read, that
//!   `'` is removed again.

use super::{encodable, field_value, new_record, options, read_scalar, record, write_scalar};
use crate::value::V;
use quadra_ir::formats::{columns, fields, kind, Column, Kind};
use quadra_ir::Model;
use std::rc::Rc;

/// The characters that make a spreadsheet read a cell as a formula.
const FORMULA: &[char] = &['=', '+', '-', '@', '\t', '\r'];

/// The rows of a CSV text: the line each starts on, and its cells.
fn rows(text: &str, sep: char) -> Result<Vec<(usize, Vec<String>)>, String> {
    let take = std::mem::take::<String>;
    let mut out = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let (mut line, mut start) = (1, 1);
    // at the start of a cell; just after the closing quote of one; the row has something
    let (mut cell_start, mut closed, mut any) = (true, false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if cell_start && c == '"' {
            let open = line;
            loop {
                match chars.next() {
                    None => return Err(format!("line {open}: a quote is never closed")),
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        cell.push('"');
                    }
                    Some('"') => break,
                    Some(ch) => {
                        if ch == '\n' {
                            line += 1;
                        }
                        cell.push(ch);
                    }
                }
            }
            (cell_start, closed, any) = (false, true, true);
        } else if c == sep {
            cells.push(take(&mut cell));
            (cell_start, closed, any) = (true, false, true);
        } else if c == '\n' || (c == '\r' && matches!(chars.peek(), Some('\n') | None)) {
            if c == '\r' {
                chars.next();
            }
            cells.push(take(&mut cell));
            let row = std::mem::take(&mut cells);
            // an empty line is no row
            if any {
                out.push((start, row));
            }
            line += 1;
            start = line;
            (cell_start, closed, any) = (true, false, false);
        } else if closed {
            return Err(format!("line {line}: text after the closing quote of a cell (a quote inside a quoted cell is doubled: `\"\"`)"));
        } else {
            cell.push(c);
            (cell_start, any) = (false, true);
        }
    }
    if any {
        cells.push(cell);
        out.push((start, cells));
    }
    Ok(out)
}

/// Reads the rows of a CSV text into records of `rec`.
pub(super) fn parse_rows(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let mut out = Vec::new();
    for (_, row) in parse_each(m, rec, text)? {
        out.push(row?);
    }
    Ok(V::List(Rc::new(out)))
}

/// Reads each row of a CSV text on its own: the line it starts on, and its record or what is
/// wrong with it. An error of the whole file (a quote never closed, a column missing from the
/// header) is the outer one.
pub(super) fn parse_each(m: &Model, rec: &str, text: &str) -> Result<super::Rows, String> {
    let e = record(m, rec)?;
    let opts = options(e)?;
    let fields = fields(m, e);
    let mut rows = rows(text, opts.sep)?.into_iter();
    // where each field's cell is in a row: its index, or none (an optional field whose
    // column the header does not have)
    let mut at: Vec<(Option<usize>, Column)> = Vec::new();
    let header = if opts.header { rows.next() } else { None };
    for (f, c) in fields.iter().zip(columns(&fields, opts.header)) {
        let c = c?;
        let index = match (&c, &header) {
            (Column::Position(p), _) => Some(p - 1),
            (Column::Name(n), Some((_, names))) => names.iter().position(|h| h.trim() == n),
            (Column::Name(_), None) => None,
        };
        let optional = matches!(f.ty, Some(quadra_ir::Ty::Optional(_)));
        if index.is_none() && !optional {
            let names: Vec<String> = header
                .as_ref()
                .map(|(_, h)| h.iter().map(|n| format!("`{}`", n.trim())).collect())
                .unwrap_or_default();
            return Err(format!(
                "line 1: the header has no column {c} (its columns: {})",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            ));
        }
        at.push((index, c));
    }
    let kinds = fields
        .iter()
        .map(|f| f.ty.as_ref().map(|ty| kind(m, ty, "csv")).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (line, cells) in rows {
        let row = (|| -> Result<V, String> {
            let mut values = Vec::new();
            for ((f, (index, column)), k) in fields.iter().zip(&at).zip(&kinds) {
                let Some(k) = k else { continue };
                let here = |why: String| format!("line {line}, column {column}: {why}");
                let optional = matches!(k, Kind::Optional(_));
                let cell = match index.map(|i| cells.get(i)) {
                    None => None,
                    Some(Some(c)) => Some(c.as_str()),
                    Some(None) if optional => None,
                    Some(None) => {
                        return Err(here(format!("missing (the row has {} cells)", cells.len())))
                    }
                };
                let k = k.unopt();
                let v = match cell {
                    None => V::None,
                    Some(c) if c.trim().is_empty() && optional => V::None,
                    Some(c) if c.trim().is_empty() && !matches!(k, Kind::Text) => {
                        return Err(here("missing".into()))
                    }
                    Some(c) => {
                        // the `'` written before a formula is not part of the text
                        let c = match c.strip_prefix('\'') {
                            Some(rest) if matches!(k, Kind::Text) && rest.starts_with(FORMULA) => {
                                rest
                            }
                            _ => c,
                        };
                        read_scalar(m, k, c, f, opts.decimal).map_err(here)?
                    }
                };
                values.push((f.name.clone(), v));
            }
            Ok(new_record(m, rec, values))
        })();
        out.push((line, row));
    }
    Ok(out)
}

/// A cell as written in the file: quoted when it holds the separator, a quote or a new line.
fn quote(cell: &str, sep: char) -> String {
    if cell.contains(sep) || cell.contains(['"', '\r', '\n']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

/// Writes records of `rec` as a CSV text: the header (when the record has one), then a row
/// per record, each line ended by `\r\n`.
pub(super) fn render_rows(m: &Model, rec: &str, rows: &[V]) -> Result<String, String> {
    let e = record(m, rec)?;
    let opts = options(e)?;
    let fields = fields(m, e);
    let cols: Vec<Column> = columns(&fields, opts.header)
        .into_iter()
        .collect::<Result<_, _>>()?;
    let width = cols
        .iter()
        .map(|c| match c {
            Column::Position(p) => *p,
            Column::Name(_) => 0,
        })
        .max()
        .unwrap_or(0)
        .max(if opts.header { cols.len() } else { 0 });
    let sep = opts.sep.to_string();
    let mut out = String::new();
    let line = |cells: Vec<String>, out: &mut String| {
        out.push_str(&cells.join(&sep));
        out.push_str("\r\n");
    };
    if opts.header {
        let mut names = Vec::new();
        for c in &cols {
            let Column::Name(n) = c else { continue };
            encodable(n, opts.encoding).map_err(|e| format!("the header: {e}"))?;
            names.push(quote(n, opts.sep));
        }
        line(names, &mut out);
    }
    for (i, r) in rows.iter().enumerate() {
        let V::Rec { fields: values, .. } = r else {
            continue;
        };
        let mut cells = vec![String::new(); width];
        for (j, (f, c)) in fields.iter().zip(&cols).enumerate() {
            let Some(ty) = &f.ty else { continue };
            let here = |why: String| format!("row {}, column {c}: {why}", i + 1);
            let k = kind(m, ty, "csv")?;
            let v = field_value(values, f);
            let text = match (&v, &k) {
                (V::None, Kind::Optional(_)) => String::new(),
                _ => {
                    let k = k.unopt();
                    let mut t = write_scalar(k, &v, f, opts.decimal).map_err(here)?;
                    if matches!(k, Kind::Text | Kind::Enum(_)) && t.starts_with(FORMULA) {
                        t.insert(0, '\'');
                    }
                    t
                }
            };
            encodable(&text, opts.encoding).map_err(here)?;
            let at = match c {
                Column::Position(p) => p - 1,
                Column::Name(_) => j,
            };
            cells[at] = quote(&text, opts.sep);
        }
        line(cells, &mut out);
    }
    Ok(out)
}
