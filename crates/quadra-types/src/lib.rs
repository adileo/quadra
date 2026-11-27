//! Type checker (phase 3).
//!
//! It is *gradual*: when it can prove an error it reports it; when it cannot decide (a unit
//! or currency known only at run time, a value returned by an untyped parameter) it trusts
//! the code and the interpreter checks at run time. What it proves:
//! - currencies: `Money<EUR> + Money<USD>`, sums over records in different currencies;
//! - units: `kg + pcs`, `Qty<kg> × Price<EUR, pcs>`, unknown units and currencies;
//! - rounding: an unrounded (`Exact`) amount or price stored in a field;
//! - optionals: `x.field` on an optional without `?.` (with narrowing);
//! - fields, methods, constructor arguments, enum values, exhaustive `match`;
//! - purity: no `now()` / `today()` in formulas, `convert()` with an explicit date.

mod check;
mod ty;

pub use check::{analyze_model, check_model, Node, Report};
pub use ty::{Num, Unit, T};

/// The words that make a `Duration` instead of a quantity (`30 days`, `8 hours`), with the
/// seconds of one. `h` and `min` stay units of measure (`Qty<h>`), converted with `.to(h)`.
pub fn duration_unit(unit: &str) -> Option<i64> {
    match unit {
        "second" | "seconds" => Some(1),
        "minute" | "minutes" => Some(60),
        "hour" | "hours" => Some(3600),
        "day" | "days" => Some(86400),
        "week" | "weeks" => Some(7 * 86400),
        _ => None,
    }
}
