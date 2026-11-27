//! Values and causes ↔ JSON (history, ledger rows, the API).

use crate::store::Cause;
use crate::V;
use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde_json::{json, Value as J};
use std::rc::Rc;
use std::str::FromStr;

pub fn to_json(v: &V) -> J {
    match v {
        V::None | V::Lambda(_) | V::Current => J::Null,
        V::Bool(b) => J::Bool(*b),
        V::Int(i) => json!(i),
        V::Dec(d) => json!({ "d": d.to_string() }),
        V::Pct(d) => json!({ "pct": d.to_string() }),
        V::Text(t) => J::String(t.to_string()),
        V::Date(d) => json!({ "date": d.to_string() }),
        V::DateTime(d) => json!({ "dt": d.format("%Y-%m-%dT%H:%M:%S").to_string() }),
        V::Duration(s) => json!({ "dur": s }),
        V::YearMonth(y, m) => json!({ "ym": format!("{y}-{m:02}") }),
        V::DateRange(a, b) => json!({ "range": [a.to_string(), b.to_string()] }),
        V::Money { amt, cur } => json!({ "money": amt.to_string(), "cur": cur.to_string() }),
        V::Price { amt, cur, per } => {
            json!({ "price": amt.to_string(), "cur": cur.to_string(), "per": per.to_string() })
        }
        V::Qty { v, unit } => json!({ "qty": v.to_string(), "unit": unit.to_string() }),
        V::Ref { ent, id } => json!({ "ref": id, "ent": ent.to_string() }),
        V::Rec { ent, fields } => {
            json!({ "rec": ent.to_string(), "fields": fields.iter().map(|(n, v)| json!([n, to_json(v)])).collect::<Vec<_>>() })
        }
        V::Variant { name, fields } => {
            json!({ "variant": name.to_string(), "fields": fields.iter().map(to_json).collect::<Vec<_>>() })
        }
        V::List(items) => json!({ "list": items.iter().map(to_json).collect::<Vec<_>>() }),
        V::Set(items) => json!({ "set": items.iter().map(to_json).collect::<Vec<_>>() }),
        V::Tuple(items) => {
            json!({ "tuple": items.iter().map(|(n, v)| json!([n, to_json(v)])).collect::<Vec<_>>() })
        }
        V::Role(r) => json!({ "role": r.to_string() }),
        V::Table(t) => json!({ "table": t.to_string() }),
        V::Event { name, fields } => {
            json!({ "event": name.to_string(), "fields": fields.iter().map(|(n, v)| json!([n, to_json(v)])).collect::<Vec<_>>() })
        }
    }
}

/// `to_json(v).to_string()` written directly (the same text: keys in order, strings escaped as
/// serde_json escapes them), without building the JSON value: a bulk import writes millions.
pub fn write_json(v: &V, out: &mut String) {
    use std::fmt::Write;
    let s = |out: &mut String, t: &str| out.push_str(&serde_json::to_string(t).unwrap_or_default());
    match v {
        V::None | V::Lambda(_) | V::Current => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Int(i) => {
            let _ = write!(out, "{i}");
        }
        V::Dec(d) => {
            let _ = write!(out, "{{\"d\":\"{d}\"}}");
        }
        V::Pct(d) => {
            let _ = write!(out, "{{\"pct\":\"{d}\"}}");
        }
        V::Text(t) => s(out, t),
        V::Date(d) => {
            let _ = write!(out, "{{\"date\":\"{d}\"}}");
        }
        V::Money { amt, cur } => {
            out.push_str("{\"cur\":");
            s(out, cur);
            let _ = write!(out, ",\"money\":\"{amt}\"}}");
        }
        V::Qty { v, unit } => {
            let _ = write!(out, "{{\"qty\":\"{v}\",\"unit\":");
            s(out, unit);
            out.push('}');
        }
        V::Ref { ent, id } => {
            out.push_str("{\"ent\":");
            s(out, ent);
            let _ = write!(out, ",\"ref\":{id}}}");
        }
        V::Variant { name, fields } if fields.is_empty() => {
            out.push_str("{\"fields\":[],\"variant\":");
            s(out, name);
            out.push('}');
        }
        V::List(items) | V::Set(items) => {
            out.push_str(if matches!(v, V::List(_)) {
                "{\"list\":["
            } else {
                "{\"set\":["
            });
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(it, out);
            }
            out.push_str("]}");
        }
        other => out.push_str(&to_json(other).to_string()),
    }
}

fn dec(s: &J) -> Decimal {
    s.as_str()
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or_default()
}

fn rc(s: &J) -> Rc<str> {
    Rc::from(s.as_str().unwrap_or(""))
}

fn pairs(a: &J) -> Vec<(String, V)> {
    a.as_array()
        .map(|items| {
            items
                .iter()
                .map(|p| (p[0].as_str().unwrap_or("").to_string(), from_json(&p[1])))
                .collect()
        })
        .unwrap_or_default()
}

pub fn from_json(j: &J) -> V {
    match j {
        J::Null => V::None,
        J::Bool(b) => V::Bool(*b),
        J::Number(n) => V::Int(n.as_i64().unwrap_or(0)),
        J::String(s) => V::Text(Rc::from(s.as_str())),
        J::Array(_) => V::None,
        J::Object(o) => {
            if let Some(d) = o.get("d") {
                V::Dec(dec(d))
            } else if let Some(d) = o.get("pct") {
                V::Pct(dec(d))
            } else if let Some(d) = o.get("date") {
                NaiveDate::from_str(d.as_str().unwrap_or(""))
                    .map(V::Date)
                    .unwrap_or(V::None)
            } else if let Some(d) = o.get("dt") {
                // stored without a zone (it is UTC); the API writes a `Z`, or an offset comes in
                let t = d.as_str().unwrap_or("");
                NaiveDateTime::parse_from_str(t.strip_suffix('Z').unwrap_or(t), "%Y-%m-%dT%H:%M:%S")
                    .ok()
                    .or_else(|| {
                        chrono::DateTime::parse_from_rfc3339(t)
                            .ok()
                            .map(|x| x.naive_utc())
                    })
                    .map(V::DateTime)
                    .unwrap_or(V::None)
            } else if let Some(d) = o.get("dur") {
                V::Duration(d.as_i64().unwrap_or(0))
            } else if let Some(ym) = o.get("ym") {
                let s = ym.as_str().unwrap_or("0-0");
                let (y, m) = s.split_once('-').unwrap_or(("0", "0"));
                V::YearMonth(y.parse().unwrap_or(0), m.parse().unwrap_or(0))
            } else if let Some(r) = o.get("range") {
                match (
                    NaiveDate::from_str(r[0].as_str().unwrap_or("")),
                    NaiveDate::from_str(r[1].as_str().unwrap_or("")),
                ) {
                    (Ok(a), Ok(b)) => V::DateRange(a, b),
                    _ => V::None,
                }
            } else if let Some(a) = o.get("money") {
                V::Money {
                    amt: dec(a),
                    cur: rc(&o["cur"]),
                }
            } else if let Some(a) = o.get("price") {
                V::Price {
                    amt: dec(a),
                    cur: rc(&o["cur"]),
                    per: rc(&o["per"]),
                }
            } else if let Some(a) = o.get("qty") {
                V::Qty {
                    v: dec(a),
                    unit: rc(&o["unit"]),
                }
            } else if let Some(id) = o.get("ref") {
                V::Ref {
                    ent: rc(&o["ent"]),
                    id: id.as_u64().unwrap_or(0),
                }
            } else if let Some(e) = o.get("rec") {
                V::Rec {
                    ent: rc(e),
                    fields: Rc::new(pairs(&o["fields"])),
                }
            } else if let Some(n) = o.get("variant") {
                let fields = o["fields"]
                    .as_array()
                    .map(|a| a.iter().map(from_json).collect())
                    .unwrap_or_default();
                V::Variant {
                    name: rc(n),
                    fields: Rc::new(fields),
                }
            } else if let Some(items) = o.get("list") {
                V::List(Rc::new(
                    items
                        .as_array()
                        .map(|a| a.iter().map(from_json).collect())
                        .unwrap_or_default(),
                ))
            } else if let Some(items) = o.get("set") {
                V::Set(Rc::new(
                    items
                        .as_array()
                        .map(|a| a.iter().map(from_json).collect())
                        .unwrap_or_default(),
                ))
            } else if let Some(items) = o.get("tuple") {
                let t = items
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|p| (p[0].as_str().map(str::to_string), from_json(&p[1])))
                            .collect()
                    })
                    .unwrap_or_default();
                V::Tuple(Rc::new(t))
            } else if let Some(r) = o.get("role") {
                V::Role(rc(r))
            } else if let Some(t) = o.get("table") {
                V::Table(rc(t))
            } else if let Some(e) = o.get("event") {
                V::Event {
                    name: rc(e),
                    fields: Rc::new(pairs(&o["fields"])),
                }
            } else {
                V::None
            }
        }
    }
}

pub fn cause_to_json(c: &Cause) -> J {
    let mut out = Vec::new();
    let mut cur = Some(c);
    while let Some(x) = cur {
        out.push(json!({ "desc": x.desc, "user": x.user, "time": x.time.format("%Y-%m-%dT%H:%M:%S").to_string() }));
        cur = x.parent.as_deref();
    }
    J::Array(out)
}

pub fn cause_from_json(j: &J) -> Rc<Cause> {
    let items = j.as_array().cloned().unwrap_or_default();
    let mut parent: Option<Rc<Cause>> = None;
    for x in items.iter().rev() {
        let time =
            NaiveDateTime::parse_from_str(x["time"].as_str().unwrap_or(""), "%Y-%m-%dT%H:%M:%S")
                .unwrap_or_default();
        parent = Some(Rc::new(Cause {
            desc: x["desc"].as_str().unwrap_or("").to_string(),
            user: x["user"].as_str().unwrap_or("").to_string(),
            time,
            parent,
        }));
    }
    parent.unwrap_or_else(|| {
        Rc::new(Cause {
            desc: "setup".into(),
            user: "system".into(),
            time: NaiveDateTime::default(),
            parent: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_json` writes the text of `to_json`, byte for byte (commit hashes depend on it).
    #[test]
    fn write_json_is_to_json_text() {
        let d = |s: &str| Decimal::from_str(s).unwrap();
        let r = |e: &str, id| V::Ref {
            ent: Rc::from(e),
            id,
        };
        let values = vec![
            V::None,
            V::Bool(true),
            V::Bool(false),
            V::Int(-42),
            V::Int(i64::MAX),
            V::Dec(d("7.50")),
            V::Dec(d("-0.001")),
            V::Pct(d("12.5")),
            V::Text(Rc::from("plain")),
            V::Text(Rc::from("quote \" back \\ tab \t nl \n ctrl \u{1} é ✓")),
            V::Date(NaiveDate::from_ymd_opt(2026, 2, 3).unwrap()),
            V::DateTime(
                NaiveDate::from_ymd_opt(2026, 2, 3)
                    .unwrap()
                    .and_hms_opt(4, 5, 6)
                    .unwrap(),
            ),
            V::Duration(3600),
            V::YearMonth(2026, 3),
            V::Money {
                amt: d("12.50"),
                cur: Rc::from("EUR"),
            },
            V::Price {
                amt: d("9.90"),
                cur: Rc::from("EUR"),
                per: Rc::from("pcs"),
            },
            V::Qty {
                v: d("3"),
                unit: Rc::from("kg\"x"),
            },
            r("Customer", 5),
            V::Variant {
                name: Rc::from("Draft"),
                fields: Rc::new(Vec::new()),
            },
            V::Variant {
                name: Rc::from("Some"),
                fields: Rc::new(vec![V::Int(1)]),
            },
            V::List(Rc::new(vec![
                r("Tag", 1),
                V::Int(2),
                V::Text(Rc::from("x")),
            ])),
            V::Set(Rc::new(vec![r("Tag", 3), r("Tag", 4)])),
            V::Set(Rc::new(Vec::new())),
            V::Role(Rc::from("Admin")),
        ];
        for v in values {
            let mut out = String::new();
            write_json(&v, &mut out);
            assert_eq!(out, to_json(&v).to_string(), "{v:?}");
        }
    }
}
