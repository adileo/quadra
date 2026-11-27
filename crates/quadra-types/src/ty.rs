//! Types used by the checker.

use std::fmt;

/// Currency or unit of a Money / Price / Qty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    /// `EUR`, `pcs`, `kg`
    Const(String),
    /// A path from a root (`self`, a parameter, a lambda parameter, `current`):
    /// `self.order.currency`, `l.product.uom`.
    Path(Vec<String>),
    /// Not known statically: compatible with everything.
    Unknown,
}

impl Unit {
    pub fn show(&self) -> String {
        match self {
            Unit::Const(c) => c.clone(),
            Unit::Path(p) => {
                let p: Vec<&str> = p
                    .iter()
                    .map(|s| {
                        if s.starts_with('#') {
                            "(…)"
                        } else {
                            s.as_str()
                        }
                    })
                    .filter(|s| *s != "self")
                    .collect();
                if p.is_empty() {
                    "self".into()
                } else {
                    p.join(".")
                }
            }
            Unit::Unknown => "?".into(),
        }
    }

    pub fn root(&self) -> Option<&str> {
        match self {
            Unit::Path(p) => p.first().map(String::as_str),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Num {
    Int,
    Decimal,
    Percent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum T {
    Unknown,
    Bool,
    Num(Num),
    /// The literal `0`: compatible with Money and Qty.
    Zero,
    Text(&'static str),
    Date,
    DateTime,
    Duration,
    DateRange,
    YearMonth,
    Money {
        cur: Unit,
        exact: bool,
    },
    Price {
        cur: Unit,
        per: Unit,
        exact: bool,
    },
    Qty {
        unit: Unit,
        exact: bool,
    },
    /// Entity, record, trait or ledger row.
    Ent(String),
    /// Named enum or inline `enum(...)`; the variant names.
    Enum {
        name: String,
        variants: Vec<String>,
    },
    /// A bare variant name whose enum is decided by the context.
    Variant(String),
    Role,
    Event(String),
    /// Static access to all records of an entity (`Stock.where(...)`).
    Table(String),
    Opt(Box<T>),
    Coll {
        elem: Box<T>,
        set: bool,
        facts: Vec<Fact>,
    },
    Tuple(Vec<(Option<String>, T)>),
    Fn {
        params: usize,
        ret: Box<T>,
    },
    None,
    /// Context object `current` (user, company).
    Current,
}

/// Known equality for the elements of a collection: `elem.<rel> == <target>`.
/// From inverse relations (`self.lines` → `l.order == self`) and `where` filters.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub rel: Vec<String>,
    pub target: Vec<String>,
}

impl T {
    pub fn text() -> T {
        T::Text("Text")
    }

    pub fn list(elem: T) -> T {
        T::Coll {
            elem: Box::new(elem),
            set: false,
            facts: Vec::new(),
        }
    }

    pub fn opt(t: T) -> T {
        match t {
            T::Opt(_) | T::Unknown | T::None => t,
            other => T::Opt(Box::new(other)),
        }
    }

    pub fn unopt(&self) -> &T {
        match self {
            T::Opt(i) => i,
            other => other,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, T::Unknown)
    }

    pub fn is_exact(&self) -> bool {
        matches!(
            self,
            T::Money { exact: true, .. }
                | T::Price { exact: true, .. }
                | T::Qty { exact: true, .. }
        )
    }

    pub fn with_exact(self, exact: bool) -> T {
        match self {
            T::Money { cur, .. } => T::Money { cur, exact },
            T::Price { cur, per, .. } => T::Price { cur, per, exact },
            T::Qty { unit, .. } => T::Qty { unit, exact },
            other => other,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, T::Num(_) | T::Zero)
    }
}

impl fmt::Display for T {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exact = |s: String, e: bool| if e { format!("Exact<{s}>") } else { s };
        match self {
            T::Unknown => write!(f, "?"),
            T::Bool => write!(f, "Bool"),
            T::Num(Num::Int) => write!(f, "Int"),
            T::Num(Num::Decimal) => write!(f, "Decimal"),
            T::Num(Num::Percent) => write!(f, "Percent"),
            T::Zero => write!(f, "0"),
            T::Text(n) => write!(f, "{n}"),
            T::Date => write!(f, "Date"),
            T::DateTime => write!(f, "DateTime"),
            T::Duration => write!(f, "Duration"),
            T::DateRange => write!(f, "DateRange"),
            T::YearMonth => write!(f, "YearMonth"),
            T::Money { cur, exact: e } => {
                write!(f, "{}", exact(format!("Money<{}>", cur.show()), *e))
            }
            T::Price { cur, per, exact: e } => write!(
                f,
                "{}",
                exact(format!("Price<{}, {}>", cur.show(), per.show()), *e)
            ),
            T::Qty { unit, exact: e } => {
                write!(f, "{}", exact(format!("Qty<{}>", unit.show()), *e))
            }
            T::Ent(n) | T::Event(n) => write!(f, "{n}"),
            T::Table(n) => write!(f, "{n} (all records)"),
            T::Enum { name, .. } => write!(f, "{name}"),
            T::Variant(v) => write!(f, "{v}"),
            T::Role => write!(f, "Role"),
            T::Opt(i) => write!(f, "{i}?"),
            T::Coll {
                elem, set: true, ..
            } => write!(f, "{{{elem}}}"),
            T::Coll { elem, .. } => write!(f, "[{elem}]"),
            T::Tuple(items) => {
                let s: Vec<String> = items
                    .iter()
                    .map(|(n, t)| match n {
                        Some(n) => format!("{n}: {t}"),
                        None => t.to_string(),
                    })
                    .collect();
                write!(f, "({})", s.join(", "))
            }
            T::Fn { .. } => write!(f, "function"),
            T::None => write!(f, "none"),
            T::Current => write!(f, "current"),
        }
    }
}
