//! Runtime values. Numbers are exact decimals: no floating point anywhere.

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use std::fmt;
use std::rc::Rc;

pub type Id = u64;

#[derive(Clone, Debug)]
pub enum V {
    None,
    Bool(bool),
    Int(i64),
    Dec(Decimal),
    /// stored as a fraction: 22% = 0.22
    Pct(Decimal),
    Text(Rc<str>),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    /// seconds
    Duration(i64),
    YearMonth(i32, u32),
    DateRange(NaiveDate, NaiveDate),
    Money {
        amt: Decimal,
        cur: Rc<str>,
    },
    Price {
        amt: Decimal,
        cur: Rc<str>,
        per: Rc<str>,
    },
    Qty {
        v: Decimal,
        unit: Rc<str>,
    },
    /// a stored record of an entity or a ledger row
    Ref {
        ent: Rc<str>,
        id: Id,
    },
    /// value object (`record`): fields by name
    Rec {
        ent: Rc<str>,
        fields: Rc<Vec<(String, V)>>,
    },
    Variant {
        name: Rc<str>,
        fields: Rc<Vec<V>>,
    },
    List(Rc<Vec<V>>),
    Set(Rc<Vec<V>>),
    Tuple(Rc<Vec<(Option<String>, V)>>),
    Lambda(Rc<Closure>),
    Role(Rc<str>),
    Table(Rc<str>),
    Event {
        name: Rc<str>,
        fields: Rc<Vec<(String, V)>>,
    },
    Current,
}

pub struct Closure {
    pub params: Vec<String>,
    pub body: std::sync::Arc<quadra_ir::Expr>,
    pub locals: Vec<(String, V)>,
    pub this: Option<V>,
    pub entity: Option<String>,
    pub company: Option<Id>,
    pub as_of: Option<u64>,
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<lambda {}>", self.params.join(", "))
    }
}

pub fn text(s: &str) -> V {
    V::Text(Rc::from(s))
}

pub fn list(v: Vec<V>) -> V {
    V::List(Rc::new(v))
}

impl V {
    /// The same value with its decimals in canonical form (`51000.00` → `51000`): sums and
    /// aggregates over whole entities are computed by the database from canonical decimals.
    pub fn normalized(self) -> V {
        match self {
            V::Dec(d) => V::Dec(d.normalize()),
            V::Pct(d) => V::Pct(d.normalize()),
            V::Money { amt, cur } => V::Money {
                amt: amt.normalize(),
                cur,
            },
            V::Price { amt, cur, per } => V::Price {
                amt: amt.normalize(),
                cur,
                per,
            },
            V::Qty { v, unit } => V::Qty {
                v: v.normalize(),
                unit,
            },
            other => other,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, V::None)
    }

    pub fn truthy(&self) -> bool {
        matches!(self, V::Bool(true))
    }

    pub fn as_ref_id(&self) -> Option<Id> {
        match self {
            V::Ref { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Elements of a collection.
    pub fn items(&self) -> Option<&[V]> {
        match self {
            V::List(v) | V::Set(v) => Some(v),
            _ => None,
        }
    }

    /// Numeric value for arithmetic and comparisons.
    pub fn num(&self) -> Option<Decimal> {
        match self {
            V::Int(i) => Some(Decimal::from(*i)),
            V::Dec(d) | V::Pct(d) => Some(*d),
            _ => None,
        }
    }
}

impl PartialEq for V {
    fn eq(&self, other: &V) -> bool {
        use V::*;
        match (self, other) {
            (None, None) => true,
            (Bool(a), Bool(b)) => a == b,
            (Text(a), Text(b)) => a == b,
            (Date(a), Date(b)) => a == b,
            (DateTime(a), DateTime(b)) => a == b,
            (Duration(a), Duration(b)) => a == b,
            (YearMonth(a, b), YearMonth(c, d)) => a == c && b == d,
            (Money { amt: a, cur: c }, Money { amt: b, cur: d }) => a == b && c == d,
            // zero is neutral: `due == 0`
            (Money { amt, .. }, Int(0)) | (Int(0), Money { amt, .. }) => amt.is_zero(),
            (Qty { v, .. }, Int(0)) | (Int(0), Qty { v, .. }) => v.is_zero(),
            (Price { amt, .. }, Int(0)) | (Int(0), Price { amt, .. }) => amt.is_zero(),
            (
                Price {
                    amt: a,
                    cur: c,
                    per: p,
                },
                Price {
                    amt: b,
                    cur: d,
                    per: q,
                },
            ) => a == b && c == d && p == q,
            (Qty { v: a, unit: u }, Qty { v: b, unit: w }) => a == b && u == w,
            (Ref { id: a, .. }, Ref { id: b, .. }) => a == b,
            (Rec { fields: a, .. }, Rec { fields: b, .. }) => a == b,
            (Variant { name: a, fields: f }, Variant { name: b, fields: g }) => a == b && f == g,
            (List(a), List(b)) | (Set(a), Set(b)) | (List(a), Set(b)) | (Set(a), List(b)) => a == b,
            (Tuple(a), Tuple(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|((_, x), (_, y))| x == y)
            }
            (Role(a), Role(b)) | (Table(a), Table(b)) => a == b,
            (a, b) => match (a.num(), b.num()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        }
    }
}

fn fmt_dec(d: &Decimal) -> String {
    let s = d.normalize().to_string();
    s
}

/// The amount of a price: at least the currency's decimals (`7.50 EUR/pcs`), more when the
/// price has them (`1.2345 EUR/pcs`).
pub fn price_amount(amt: &Decimal, decimals: u32) -> String {
    let n = amt.normalize();
    format!("{:.*}", n.scale().max(decimals) as usize, n)
}

impl fmt::Display for V {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            V::None => write!(f, "none"),
            V::Bool(b) => write!(f, "{b}"),
            V::Int(i) => write!(f, "{i}"),
            V::Dec(d) => write!(f, "{}", fmt_dec(d)),
            V::Pct(d) => write!(f, "{}%", fmt_dec(&(d * Decimal::from(100)))),
            V::Text(t) => write!(f, "{t}"),
            V::Date(d) => write!(f, "{d}"),
            V::DateTime(d) => write!(f, "{}", d.format("%Y-%m-%d %H:%M")),
            V::Duration(s) => write!(f, "{}", duration_text(*s)),
            V::YearMonth(y, m) => write!(f, "{y}-{m:02}"),
            V::DateRange(a, b) => write!(f, "{a}..{b}"),
            V::Money { amt, cur } => write!(f, "{:.2} {cur}", amt),
            V::Price { amt, cur, per } => write!(f, "{} {cur}/{per}", price_amount(amt, 2)),
            V::Qty { v, unit } => write!(f, "{} {unit}", fmt_dec(v)),
            V::Ref { ent, id } => write!(f, "{ent}#{id}"),
            V::Rec { ent, .. } => write!(f, "{ent}(…)"),
            V::Variant { name, fields } => {
                if fields.is_empty() {
                    write!(f, "{name}")
                } else {
                    write!(
                        f,
                        "{name}({})",
                        fields
                            .iter()
                            .map(|x| x.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
            V::List(v) | V::Set(v) => write!(
                f,
                "[{}]",
                v.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            V::Tuple(v) => write!(
                f,
                "({})",
                v.iter()
                    .map(|(n, x)| match n {
                        Some(n) => format!("{n}: {x}"),
                        None => x.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            V::Lambda(_) => write!(f, "<function>"),
            V::Role(r) => write!(f, "{r}"),
            V::Table(t) => write!(f, "{t}"),
            V::Event { name, .. } => write!(f, "{name}(…)"),
            V::Current => write!(f, "current"),
        }
    }
}

/// A duration as people read it, and as the API reads it back: `1 day`, `3 days` (whole days),
/// `8 h`, `1 h 30 min`, `45 min`, `0 h`.
pub fn duration_text(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "" };
    let s = secs.unsigned_abs();
    if s == 0 {
        return "0 h".into();
    }
    if s % 86400 == 0 {
        let n = s / 86400;
        return format!("{sign}{n} {}", if n == 1 { "day" } else { "days" });
    }
    let (h, m) = (s / 3600, (s % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{sign}{m} min"),
        (h, 0) => format!("{sign}{h} h"),
        (h, m) => format!("{sign}{h} h {m} min"),
    }
}
