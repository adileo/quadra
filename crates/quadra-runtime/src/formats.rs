//! Records read from and written to an external format: `record ShopOrder from json:`.
//!
//! `ShopOrder.parse(text)` reads a document into a record, `order.json()` writes one. Both are
//! pure: the same text gives the same record and the same record the same text, so they can
//! be used in functions, in tests and to build what an `outbound` sends.
//!
//! A field is found under its name, or under `at "a.b.c"` (a path of keys in nested objects).
//! Keys the record does not declare are ignored when reading. The types a field may have:
//!
//! | Type | JSON |
//! |---|---|
//! | `Text`, `Code`, `Email`, `Phone`, `VatNumber`, `Barcode` | a string (a number is read as its text, for ids too large for a number) |
//! | `Int` | an integer (or a string of one) |
//! | `Decimal` | a number, written exactly; read from a number or a string |
//! | `Bool` | `true` / `false` |
//! | `Date`, `DateTime` | `"2026-11-27"`, `"2026-11-27T10:14:03Z"` (an instant without a zone is read as UTC) |
//! | an enum | the name of the variant |
//! | another `record … from json` | an object |
//! | `[T]` | an array |
//! | `T?` | missing or `null` when none |

use crate::value::V;
use chrono::{NaiveDate, NaiveDateTime};
use quadra_ir::{Entity, Model};
use rust_decimal::Decimal;
use serde_json::Value as J;
use std::rc::Rc;
use std::str::FromStr;

pub use quadra_ir::formats::{kind, place, Kind, FORMATS};

fn record<'m>(m: &'m Model, name: &str) -> Result<&'m Entity, String> {
    m.entity(name)
        .filter(|e| e.format.is_some())
        .ok_or_else(|| format!("`{name}` is not a record `from json`"))
}

/// Reads a JSON document into a record of `rec`.
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
    for f in m.all_fields(e) {
        let Some(ty) = &f.ty else { continue };
        if f.formula.is_some() {
            continue;
        }
        let keys = place(f);
        let mut cur = Some(doc);
        for k in &keys {
            cur = cur.and_then(|c| c.get(k));
        }
        let k = kind(m, ty)?;
        let here = format!("{at}.{}", keys.join("."));
        let v = read(m, &k, cur.unwrap_or(&J::Null), &here)?;
        fields.push((f.name.clone(), v));
    }
    Ok(V::Rec {
        ent: Rc::from(rec),
        fields: Rc::new(fields),
    })
}

fn read(m: &Model, k: &Kind, j: &J, at: &str) -> Result<V, String> {
    if let Kind::Optional(inner) = k {
        return if j.is_null() {
            Ok(V::None)
        } else {
            read(m, inner, j, at)
        };
    }
    if j.is_null() {
        return Err(format!("{at}: missing"));
    }
    let bad = |what: &str| Err(format!("{at}: {what} was expected, not {j}"));
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
            let known = m
                .enum_def(name)
                .is_some_and(|e| e.variants.iter().any(|v| v.name == s));
            if !known {
                return Err(format!("{at}: `{s}` is not a value of {name}"));
            }
            Ok(V::Variant {
                name: Rc::from(s),
                fields: Rc::new(Vec::new()),
            })
        }
        Kind::Record(r) => read_record(m, r, j, at),
        Kind::List(inner) => match j {
            J::Array(a) => Ok(V::List(Rc::new(
                a.iter()
                    .enumerate()
                    .map(|(i, x)| read(m, inner, x, &format!("{at}[{i}]")))
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
    write_value(m, None, v, &mut out)?;
    Ok(out)
}

fn write_value(m: &Model, k: Option<&Kind>, v: &V, out: &mut String) -> Result<(), String> {
    let s = |out: &mut String, t: &str| out.push_str(&serde_json::to_string(t).unwrap_or_default());
    match v {
        V::None => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Int(i) => {
            if matches!(k, Some(Kind::Text)) {
                s(out, &i.to_string())
            } else {
                out.push_str(&i.to_string())
            }
        }
        V::Dec(d) | V::Pct(d) => out.push_str(&d.normalize().to_string()),
        V::Text(t) => s(out, t),
        V::Date(d) => s(out, &d.format("%Y-%m-%d").to_string()),
        V::DateTime(t) => s(out, &t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
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
                write_value(m, inner.as_ref(), x, out)?;
            }
            out.push(']');
        }
        V::Rec { ent, fields } => {
            let e = record(m, ent)?;
            // nested objects for the `at` paths, in the order of the fields
            let mut tree = Tree::default();
            for f in m.all_fields(e) {
                let Some(ty) = &f.ty else { continue };
                if f.formula.is_some() {
                    continue;
                }
                let value = fields
                    .iter()
                    .find(|(n, _)| n == &f.name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(V::None);
                let k = kind(m, ty)?;
                // an optional without a value is left out
                if matches!(value, V::None) && matches!(k, Kind::Optional(_)) {
                    continue;
                }
                let k = match k {
                    Kind::Optional(i) => *i,
                    k => k,
                };
                let mut text = String::new();
                write_value(m, Some(&k), &value, &mut text)?;
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
