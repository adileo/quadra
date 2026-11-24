//! Values and causes as stored: the JSON encoding lives in the runtime (shared with the
//! API and the browser), the SQL helpers here.

pub use quadra_runtime::codec::*;

use crate::conn::Sql;
use quadra_runtime::V;
use rust_decimal::Decimal;

/// Text of a decimal for NUMERIC columns, with its scale (`7.50` stays `7.50`): a projection
/// row reads back exactly as the history has the value.
pub fn dec_text(d: &Decimal) -> Sql {
    Sql::Text(d.to_string())
}

pub fn date_text(v: &V) -> Sql {
    match v {
        V::Date(d) => Sql::Text(d.to_string()),
        V::DateTime(d) => Sql::Text(d.format("%Y-%m-%d %H:%M:%S").to_string()),
        _ => Sql::Null,
    }
}
