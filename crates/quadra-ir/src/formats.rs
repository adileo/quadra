//! What the types of a record's fields are in an external format (`record X from json:`):
//! shared by the checker, which refuses a field a format cannot hold, and the runtime, which
//! reads and writes documents (`quadra_runtime::formats`).

use crate::{Model, Ty};

/// The formats a record may declare (`from …`).
pub const FORMATS: &[&str] = &["json"];

const TEXT: &[&str] = &["Text", "Code", "Email", "Phone", "VatNumber", "Barcode"];

/// What a field's type is, for a format: `None` when the format cannot hold it.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    Int,
    Decimal,
    Bool,
    Date,
    DateTime,
    Enum(String),
    Record(String),
    List(Box<Kind>),
    Optional(Box<Kind>),
}

/// The kind of a field type in a `from json` record, or why it cannot be one.
pub fn kind(m: &Model, ty: &Ty) -> Result<Kind, String> {
    match ty {
        Ty::Optional(t) => Ok(Kind::Optional(Box::new(kind(m, t)?))),
        Ty::List(t) => Ok(Kind::List(Box::new(kind(m, t)?))),
        Ty::Path { segments, args, .. } if segments.len() == 1 && args.is_empty() => {
            let n = segments[0].as_str();
            if TEXT.contains(&n) {
                return Ok(Kind::Text);
            }
            match n {
                "Int" => Ok(Kind::Int),
                "Decimal" => Ok(Kind::Decimal),
                "Bool" => Ok(Kind::Bool),
                "Date" => Ok(Kind::Date),
                "DateTime" => Ok(Kind::DateTime),
                _ if m.enum_def(n).is_some() => Ok(Kind::Enum(n.to_string())),
                _ => match m.entity(n) {
                    Some(e) if e.format.as_deref() == Some("json") => Ok(Kind::Record(n.to_string())),
                    Some(_) => Err(format!("`{n}` is not a record `from json`: a document holds values, not records of the database (send its id, or a record `from json` made of its fields)")),
                    None => Err(format!("`{n}` cannot be read from or written to JSON")),
                },
            }
        }
        _ => Err("this type cannot be read from or written to JSON (Text, Int, Decimal, Bool, Date, DateTime, an enum, a record `from json`, a list or an optional of them)".into()),
    }
}

/// The keys of a field's place in the document.
pub fn place(f: &crate::Field) -> Vec<String> {
    match &f.at {
        Some(p) => p.split('.').map(|s| s.to_string()).collect(),
        None => vec![f.name.clone()],
    }
}
