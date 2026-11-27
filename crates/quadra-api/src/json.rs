//! Values between the engine and the API.
//!
//! Every cell travels as `{ "v": <canonical value>, "d": "<display text>" }`: the client shows
//! `d` and edits `v`. Canonical values: strings, numbers, booleans, null, and tagged objects
//! for amounts and references (`{"money": "5.40", "cur": "EUR"}`, `{"ref": 12, "ent": "Order"}`).
//! Inputs are decoded against the field's type; amounts may be written as Quadra literals
//! (`"5.40 EUR"`, `"4 pcs"`, `"1.50 EUR/pcs"`, `"10%"`), which are parsed, never executed.

use quadra_ir::{EntityKind, Model, Ty};
use quadra_runtime::{Interp, V};
use quadra_types::{Num, Unit, T};
use rust_decimal::Decimal;
use serde_json::{json, Value as J};
use std::rc::Rc;
use std::str::FromStr;

/// A value as the API sends it: the codec's JSON, with every instant marked as UTC
/// (`{"dt": "2026-11-24T09:00:00Z"}`), so that a client never reads it in its own zone.
pub fn wire(v: &V) -> J {
    fn mark(j: &mut J) {
        match j {
            J::Object(o) => {
                if o.len() == 1 {
                    if let Some(J::String(s)) = o.get_mut("dt") {
                        if !s.ends_with('Z') {
                            s.push('Z');
                        }
                        return;
                    }
                }
                o.values_mut().for_each(mark);
            }
            J::Array(a) => a.iter_mut().for_each(mark),
            _ => {}
        }
    }
    let mut j = quadra_runtime::codec::to_json(v);
    mark(&mut j);
    j
}

pub fn cell(interp: &mut Interp, v: &V) -> J {
    let mut c = json!({ "v": wire(v), "d": interp.display(v) });
    // a list of records: the name of each (`d` shows the first three)
    if let V::List(items) | V::Set(items) = v {
        if !items.is_empty() && items.iter().all(|x| matches!(x, V::Ref { .. })) {
            c["names"] = json!(items
                .iter()
                .take(500)
                .map(|x| interp.display(x))
                .collect::<Vec<_>>());
        }
    }
    c
}

/// How the client should show and edit a field.
pub fn kind(t: &T) -> J {
    match t {
        T::Opt(i) => kind(i),
        T::Bool => json!({ "kind": "bool" }),
        T::Num(Num::Int) => json!({ "kind": "int" }),
        T::Num(Num::Percent) => json!({ "kind": "percent" }),
        T::Num(_) | T::Zero => json!({ "kind": "decimal" }),
        T::Text(_) => json!({ "kind": "text" }),
        T::Date => json!({ "kind": "date" }),
        T::DateTime => json!({ "kind": "datetime" }),
        T::Duration => json!({ "kind": "duration" }),
        // a fixed currency or unit (`EUR`, `pcs`), or one taken from the record (`order.currency`, `uom`)
        T::Money { cur, .. } => {
            json!({ "kind": "money", "currency": fixed(cur), "currency_from": from(cur) })
        }
        T::Price { cur, per, .. } => {
            json!({ "kind": "price", "currency": fixed(cur), "per": fixed(per), "currency_from": from(cur), "per_from": from(per) })
        }
        T::Qty { unit, .. } => {
            json!({ "kind": "qty", "unit": fixed(unit), "unit_from": from(unit) })
        }
        T::Ent(e) => json!({ "kind": "ref", "entity": e }),
        T::Enum { variants, .. } => json!({ "kind": "enum", "variants": variants }),
        T::Role => json!({ "kind": "role" }),
        T::Coll { elem, set, .. } => {
            json!({ "kind": if *set { "set" } else { "list" }, "of": kind(elem) })
        }
        other => json!({ "kind": "other", "type": other.to_string() }),
    }
}

fn fixed(u: &Unit) -> Option<String> {
    matches!(u, Unit::Const(_)).then(|| u.show())
}

fn from(u: &Unit) -> Option<String> {
    matches!(u, Unit::Path(_)).then(|| u.show())
}

fn err<T>(m: impl Into<String>) -> Result<T, String> {
    Err(m.into())
}

/// A literal written as text: `5.40 EUR`, `-3 pcs`, `1.50 EUR/pcs`, `10%`, `2026-11-21`,
/// `2026-11-21T09:30`, `30 days`, a plain number. Read by fixed rules: never evaluated.
pub fn literal(interp: &mut Interp, text: &str) -> Result<V, String> {
    let t = text.trim();
    if t.len() > 100 {
        return err("value too long");
    }
    let number = |s: &str| -> Option<Decimal> {
        let s = s.replace('_', "");
        let ok = !s.is_empty()
            && s.chars()
                .enumerate()
                .all(|(i, c)| c.is_ascii_digit() || c == '.' || (i == 0 && c == '-'));
        if ok {
            Decimal::from_str(&s).ok()
        } else {
            None
        }
    };
    if let Some(d) = number(t) {
        return Ok(V::Dec(d));
    }
    if let Some(p) = t.strip_suffix('%').and_then(|p| number(p.trim())) {
        return Ok(V::Pct(p / Decimal::from(100)));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Ok(V::Date(d));
    }
    // an instant: with its offset (`2026-11-24T10:00:00+01:00`, `…Z`), or without one, on the
    // project's clock
    if t.len() >= 16
        && t.as_bytes().get(4) == Some(&b'-')
        && t.as_bytes()
            .get(10)
            .is_some_and(|b| *b == b'T' || *b == b' ')
    {
        if let Some(d) = quadra_runtime::time::parse_instant(t, interp.tz) {
            return Ok(V::DateTime(d));
        }
    }
    let ident = |s: &str| {
        !s.is_empty()
            && s.len() <= 12
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
    };
    if let Some((n, unit)) = t.split_once(' ') {
        let (n, unit) = (n.trim(), unit.trim());
        if let Some(v) = number(n) {
            if let Some((cur, per)) = unit.split_once('/') {
                if ident(cur) && ident(per) {
                    return Ok(V::Price {
                        amt: v,
                        cur: Rc::from(cur),
                        per: Rc::from(per),
                    });
                }
            }
            if let Some(secs) = quadra_types::duration_unit(unit) {
                return Ok(V::Duration(
                    (v * Decimal::from(secs))
                        .trunc()
                        .to_string()
                        .parse()
                        .unwrap_or(0),
                ));
            }
            if ident(unit) {
                let iso = unit.len() == 3 && unit.chars().all(|c| c.is_ascii_uppercase());
                return Ok(if iso {
                    V::Money {
                        amt: v,
                        cur: Rc::from(unit),
                    }
                } else {
                    V::Qty {
                        v,
                        unit: Rc::from(unit),
                    }
                });
            }
        }
    }
    err(format!(
        "`{t}` is not a value (examples: 5.40 EUR, 4 pcs, 1.50 EUR/pcs, 10%, 2026-11-21)"
    ))
}

/// A duration as people type it, in seconds: `8 h`, `8h`, `1 h 30 min`, `1h30`, `1:30`,
/// `1.5 h`, `90 min`, `2 days`, `1 day 4 h`, `1 week`, `8 hours` (the language's words).
/// A number alone is refused: it could be hours or days.
pub fn duration(text: &str) -> Result<i64, String> {
    let t = text.trim().to_lowercase();
    let help = || {
        format!(
            "`{}` is not a duration (examples: 8 h, 1 h 30 min, 1:30, 2 days)",
            text.trim()
        )
    };
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, t.as_str()),
    };
    if body.is_empty() {
        return Err(help());
    }
    // hours:minutes
    if let Some((h, m)) = body.split_once(':') {
        let (h, m) = (h.trim(), m.trim());
        if !h.is_empty()
            && h.chars().all(|c| c.is_ascii_digit())
            && m.len() == 2
            && m.chars().all(|c| c.is_ascii_digit())
        {
            let (h, m): (i64, i64) = (
                h.parse().map_err(|_| help())?,
                m.parse().map_err(|_| help())?,
            );
            if m >= 60 {
                return Err(help());
            }
            let s = h * 3600 + m * 60;
            return Ok(if neg { -s } else { s });
        }
        return Err(help());
    }
    // a number and a unit, again and again: "1 day 4 h", "1h30m", "1h30"
    let unit_secs = |u: &str| -> Option<i64> {
        quadra_types::duration_unit(u).or(match u {
            "w" | "wk" | "wks" => Some(7 * 86400),
            "d" => Some(86400),
            "h" | "hr" | "hrs" => Some(3600),
            "m" | "min" | "mins" => Some(60),
            "s" | "sec" | "secs" | "second" | "seconds" => Some(1),
            _ => None,
        })
    };
    let chars: Vec<char> = body.chars().collect();
    let (mut i, mut total, mut last) = (0usize, Decimal::ZERO, None::<i64>);
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',') {
            i += 1;
        }
        let num: String = chars[start..i].iter().collect::<String>().replace(',', ".");
        if num.is_empty() {
            return Err(help());
        }
        let n = Decimal::from_str(&num).map_err(|_| help())?;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let ustart = i;
        while i < chars.len() && chars[i].is_alphabetic() {
            i += 1;
        }
        let unit: String = chars[ustart..i].iter().collect();
        let secs = if unit.is_empty() {
            // "1h30": the minutes after the hours
            match last {
                Some(3600) if i >= chars.len() => 60,
                _ => {
                    return Err(format!(
                        "`{}` needs a unit: 8 h, 30 min, 2 days",
                        text.trim()
                    ))
                }
            }
        } else {
            unit_secs(&unit).ok_or_else(help)?
        };
        total += n * Decimal::from(secs);
        last = Some(secs);
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
    }
    let s: i64 = total.trunc().to_string().parse().map_err(|_| help())?;
    Ok(if neg { -s } else { s })
}

/// Decode an input value for a field of `ent`. References may point to records created
/// earlier in the same request (`{"ref": "tmp1"}` → `created`).
pub fn decode(
    interp: &mut Interp,
    model: &Model,
    ty: Option<&Ty>,
    j: &J,
    created: &dyn Fn(&str) -> Option<V>,
) -> Result<V, String> {
    let ty = match ty {
        Some(Ty::Optional(t)) => Some(t.as_ref()),
        t => t,
    };
    if j.is_null() {
        return Ok(V::None);
    }
    // collections: each element against the element type
    if let Some(Ty::List(inner) | Ty::Set(inner)) = ty {
        let items = j.as_array().ok_or("expected a list")?;
        let mut out = Vec::new();
        for it in items {
            out.push(decode(interp, model, Some(inner), it, created)?);
        }
        return Ok(if matches!(ty, Some(Ty::Set(_))) {
            V::Set(Rc::new(out))
        } else {
            V::List(Rc::new(out))
        });
    }
    let name = match ty {
        Some(Ty::Path { segments, .. }) => segments.last().cloned().unwrap_or_default(),
        _ => String::new(),
    };
    // references and new child records
    if let Some(target) = model
        .entity(&name)
        .filter(|e| e.kind != EntityKind::Record || e.is_ledger)
    {
        return match j {
            J::Number(n) => reference(
                interp,
                &target.name,
                n.as_u64().ok_or("expected a record id")?,
            ),
            J::Object(o) if o.contains_key("ref") && o["ref"].is_string() => {
                created(o["ref"].as_str().unwrap())
                    .ok_or_else(|| format!("unknown reference `{}`", o["ref"]))
            }
            J::Object(o) if o.contains_key("ref") || o.contains_key("id") => {
                let id = o
                    .get("ref")
                    .or_else(|| o.get("id"))
                    .and_then(|v| v.as_u64())
                    .ok_or("expected a record id")?;
                reference(interp, &target.name, id)
            }
            J::Object(o) => {
                // a new record given inline (order lines in an order)
                let tname = target.name.clone();
                let args = decode_fields(interp, model, &tname, o, created)?;
                interp.create(&tname, args).map_err(describe)
            }
            _ => err(format!("expected a {name} (an id)")),
        };
    }
    if model
        .entity(&name)
        .map_or(false, |e| e.kind == EntityKind::Record)
    {
        let o = j.as_object().ok_or_else(|| format!("expected a {name}"))?;
        let args = decode_fields(interp, model, &name, o, created)?;
        return interp.create(&name, args).map_err(describe);
    }
    Ok(match (name.as_str(), j) {
        (_, J::Bool(b)) => V::Bool(*b),
        ("Int", J::Number(n)) => V::Int(n.as_i64().ok_or("expected an integer")?),
        (_, J::Number(n)) => {
            V::Dec(Decimal::from_str(&n.to_string()).map_err(|_| "expected a number")?)
        }
        ("Text" | "Code" | "Email" | "Phone" | "Url", J::String(s)) => {
            if s.len() > 10_000 {
                return err("text too long");
            }
            V::Text(Rc::from(s.as_str()))
        }
        ("Decimal", J::String(s)) => {
            V::Dec(Decimal::from_str(s).map_err(|_| format!("`{s}` is not a number"))?)
        }
        (_, J::String(s)) => {
            // an enum value, or a literal
            let variants: Vec<String> = match ty {
                Some(t) => quadra_ir::inline_enum(t)
                    .map(|vs| vs.iter().map(|v| v.name.clone()).collect())
                    .or_else(|| {
                        model
                            .enum_def(&name)
                            .map(|e| e.variants.iter().map(|v| v.name.clone()).collect())
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if variants.iter().any(|v| v == s) {
                V::Variant {
                    name: Rc::from(s.as_str()),
                    fields: Rc::new(Vec::new()),
                }
            } else if !variants.is_empty() {
                return err(format!("`{s}` is not one of {}", variants.join(", ")));
            } else if name == "Duration" {
                V::Duration(duration(s)?)
            } else if matches!(
                name.as_str(),
                "Money" | "Qty" | "Price" | "Percent" | "Int" | "Decimal" | "Date" | "DateTime"
            ) {
                literal(interp, s)?
            } else {
                // a text type of the project (`VatNumber`, `Iban`…): its `where` checks it
                if s.len() > 10_000 {
                    return err("text too long");
                }
                V::Text(Rc::from(s.as_str()))
            }
        }
        (_, J::Object(_)) => quadra_runtime::codec::from_json(j),
        _ => return err("unsupported value"),
    })
}

fn reference(interp: &mut Interp, ent: &str, id: u64) -> Result<V, String> {
    if !interp.store.exists(id, None) {
        return err(format!("{ent}#{id} does not exist"));
    }
    let actual = interp
        .store
        .get(id)
        .map(|r| r.ent.to_string())
        .unwrap_or_default();
    let ok = actual == ent
        || interp
            .m
            .entity(&actual)
            .map_or(false, |e| interp.m.implements(e, ent));
    if !ok {
        return err(format!("#{id} is a {actual}, not a {ent}"));
    }
    Ok(V::Ref {
        ent: Rc::from(actual.as_str()),
        id,
    })
}

pub fn decode_fields(
    interp: &mut Interp,
    model: &Model,
    ent: &str,
    o: &serde_json::Map<String, J>,
    created: &dyn Fn(&str) -> Option<V>,
) -> Result<Vec<(String, V)>, String> {
    let e = model
        .entity(ent)
        .ok_or_else(|| format!("unknown entity `{ent}`"))?;
    let mut out = Vec::new();
    for (k, v) in o {
        let f = model
            .find_field(e, k)
            .ok_or_else(|| format!("`{ent}` has no field `{k}`"))?;
        if f.formula.is_some() {
            return err(format!("`{ent}.{k}` is a formula: it cannot be written"));
        }
        let val = decode(interp, model, f.ty.as_ref(), v, created)
            .map_err(|m| format!("{ent}.{k}: {m}"))?;
        out.push((k.clone(), val));
    }
    Ok(out)
}

pub fn describe(f: quadra_runtime::Flow) -> String {
    match f {
        quadra_runtime::Flow::Fail(m) => m,
        quadra_runtime::Flow::Return(_) => "unexpected return".into(),
        quadra_runtime::Flow::Conflict(c) => format!(
            "changed by others: {}",
            c.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::duration;

    /// An instant written with an offset or in UTC is the same; one without an offset is on the
    /// project's clock; the API sends it back with its zone.
    #[test]
    fn instants_with_and_without_an_offset() {
        let mut m = quadra_ir::Model::default();
        m.timezone = "Europe/Rome".into();
        let mut i = quadra_runtime::Interp::new(&m);
        let a = super::literal(&mut i, "2026-11-24T10:00:00+01:00").unwrap();
        let b = super::literal(&mut i, "2026-11-24T09:00:00Z").unwrap();
        let c = super::literal(&mut i, "2026-11-24T10:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c, "no offset: Rome");
        assert_eq!(
            super::wire(&a),
            serde_json::json!({ "dt": "2026-11-24T09:00:00Z" })
        );
        // inside a list too
        let list =
            super::wire(&quadra_runtime::V::List(std::rc::Rc::new(vec![a.clone()]))).to_string();
        assert!(list.contains("\"2026-11-24T09:00:00Z\""), "{list}");
        // what the API sent can come back as it is
        assert_eq!(quadra_runtime::codec::from_json(&super::wire(&a)), a);
        assert_eq!(
            i.display(&a),
            "2026-11-24 10:00",
            "shown on the project's clock"
        );
    }

    #[test]
    fn durations_as_people_type_them() {
        for (text, secs) in [
            ("8 h", 8 * 3600),
            ("8h", 8 * 3600),
            ("8 hours", 8 * 3600),
            ("1 h 30 min", 5400),
            ("1h30", 5400),
            ("1h30m", 5400),
            ("1:30", 5400),
            ("1.5 h", 5400),
            ("1,5 h", 5400),
            ("90 min", 5400),
            ("2 days", 2 * 86400),
            ("1 day 4 h", 86400 + 4 * 3600),
            ("1 day, 4 h", 86400 + 4 * 3600),
            ("1 week", 7 * 86400),
            ("45 min", 2700),
            ("0 h", 0),
            ("-2 h", -7200),
        ] {
            assert_eq!(duration(text), Ok(secs), "{text}");
        }
        for bad in ["8", "", "h", "8 parsecs", "1:75", "1 h 30 parsecs"] {
            assert!(duration(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(
            duration("8").unwrap_err(),
            "`8` needs a unit: 8 h, 30 min, 2 days"
        );
    }

    #[test]
    fn what_a_duration_shows_is_read_back() {
        for secs in [
            0,
            60,
            2700,
            3600,
            5400,
            8 * 3600,
            26 * 3600,
            86400,
            3 * 86400,
            -5400,
        ] {
            let shown = quadra_runtime::value::duration_text(secs);
            assert_eq!(duration(&shown), Ok(secs), "{shown}");
        }
    }
}
