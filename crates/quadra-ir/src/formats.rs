//! What the fields of a record `from` an external format are (`record X from json:`,
//! `from xml`, `from csv(…)`, `from fixed`): shared by the checker, which refuses what a format
//! cannot hold, and the runtime, which reads and writes documents and files
//! (`quadra_runtime::formats`).
//!
//! Where a field is, in each format:
//!
//! - `json`: a document; a field is under its name or under `at "a.b"` (keys of nested objects).
//! - `xml`: a document; a field is the child element of its name, or `at "a/b"` (elements),
//!   `at "a/@id"` (an attribute), `at "."` (the text of the record's own element). Prefixes
//!   come from the record's `ns prefix "uri"` lines.
//! - `csv`: rows; a field is the column of its name (or `at "NAME"`) in the header, or without
//!   a header its position (`at "3"`, from 1). A cell holds one value: no lists, no records.
//! - `fixed`: lines; a field is `at "11..20"`, columns from 1, both included. One value per
//!   field, as in CSV.

use crate::{Entity, Field, Model, OptionValue, Span, Ty};

/// The formats a record may declare (`from …`).
pub const FORMATS: &[&str] = &["json", "xml", "csv", "fixed"];

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

impl Kind {
    /// The kind without its optional.
    pub fn unopt(&self) -> &Kind {
        match self {
            Kind::Optional(k) => k,
            k => k,
        }
    }
}

/// The name of a format in messages.
pub fn title(fmt: &str) -> &'static str {
    match fmt {
        "xml" => "XML",
        "csv" => "CSV",
        "fixed" => "a fixed-width file",
        _ => "JSON",
    }
}

/// The kind of a field type in a record `from fmt`, or why the format cannot hold it.
pub fn kind(m: &Model, ty: &Ty, fmt: &str) -> Result<Kind, String> {
    let k = kind_of(m, ty, fmt)?;
    // a row of CSV or of a fixed-width file is flat: one value per cell
    if matches!(fmt, "csv" | "fixed") {
        let cell = if fmt == "csv" { "a cell" } else { "a column" };
        match k.unopt() {
            Kind::List(_) => {
                return Err(format!(
                    "{cell} of {} holds one value, not a list",
                    title(fmt)
                ))
            }
            Kind::Record(_) => {
                return Err(format!(
                    "{cell} of {} holds one value, not a record (put its fields in the row)",
                    title(fmt)
                ))
            }
            _ => {}
        }
    }
    // a list of XML is the repeated elements (or attributes) of a path
    if fmt == "xml" {
        if let Kind::List(inner) = k.unopt() {
            if matches!(**inner, Kind::List(_) | Kind::Optional(_)) {
                return Err(
                    "a list of XML is repeated elements: of values or records, not of lists or optionals"
                        .into(),
                );
            }
        }
    }
    Ok(k)
}

fn kind_of(m: &Model, ty: &Ty, fmt: &str) -> Result<Kind, String> {
    let what = title(fmt);
    match ty {
        Ty::Optional(t) => Ok(Kind::Optional(Box::new(kind_of(m, t, fmt)?))),
        Ty::List(t) => Ok(Kind::List(Box::new(kind_of(m, t, fmt)?))),
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
                    Some(e) if e.format.as_deref() == Some(fmt) => Ok(Kind::Record(n.to_string())),
                    Some(e) if e.format.is_some() => Err(format!("`{n}` is a record `from {}`: a record inside a record `from {fmt}` is `from {fmt}` too", e.format.as_deref().unwrap_or_default())),
                    Some(_) => Err(format!("`{n}` is not a record `from {fmt}`: a document holds values, not records of the database (send its id, or a record `from {fmt}` made of its fields)")),
                    None => Err(format!("`{n}` cannot be read from or written to {what}")),
                },
            }
        }
        _ => Err(format!("this type cannot be read from or written to {what} (Text, Int, Decimal, Bool, Date, DateTime, an enum, a record `from {fmt}`, a list or an optional of them)")),
    }
}

/// The keys of a field's place in a JSON document.
pub fn place(f: &crate::Field) -> Vec<String> {
    match &f.at {
        Some(p) => p.split('.').map(|s| s.to_string()).collect(),
        None => vec![f.name.clone()],
    }
}

/// The fields a record reads and writes: the stored ones with a type, in their order.
pub fn fields<'m>(m: &'m Model, e: &'m Entity) -> Vec<&'m Field> {
    m.all_fields(e)
        .into_iter()
        .filter(|f| f.ty.is_some() && f.formula.is_none())
        .collect()
}

// ---- options ---------------------------------------------------------------------------

/// How the characters of a file are written as bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// ISO-8859-1: one byte per character, the characters up to U+00FF
    Latin1,
}

impl Encoding {
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Latin1 => "latin1",
        }
    }
}

/// The options of a record's format, with their defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// csv: the separator of the cells (`,`)
    pub sep: char,
    /// csv: the decimal separator (`.`)
    pub decimal: char,
    /// csv: the first row names the columns (`true`)
    pub header: bool,
    /// csv, fixed: the encoding of the file (`utf-8`)
    pub encoding: Encoding,
    /// xml: the name of the root element (the record's name); when given, a document with
    /// another root is refused
    pub root: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sep: ',',
            decimal: '.',
            header: true,
            encoding: Encoding::Utf8,
            root: None,
        }
    }
}

/// The options a format takes.
fn known_options(fmt: &str) -> &'static [&'static str] {
    match fmt {
        "csv" => &["sep", "decimal", "header", "encoding"],
        "fixed" => &["encoding"],
        "xml" => &["root"],
        _ => &[],
    }
}

/// The options of a record `from` a format, or the first one that is wrong (where and why).
pub fn options(e: &Entity) -> Result<Options, (Span, String)> {
    let fmt = e.format.as_deref().unwrap_or("json");
    let mut o = Options::default();
    let known = known_options(fmt);
    for opt in &e.format_options {
        let err = |why: String| Err((opt.span, why));
        if !known.contains(&opt.name.as_str()) {
            return err(if known.is_empty() {
                format!("`{fmt}` takes no option")
            } else {
                format!(
                    "`{}` is not an option of `{fmt}` ({})",
                    opt.name,
                    known.join(", ")
                )
            });
        }
        let text = match &opt.value {
            OptionValue::Text(t) => Some(t.as_str()),
            _ => None,
        };
        let one_char = |t: Option<&str>| -> Option<char> {
            let mut cs = t?.chars();
            let c = cs.next()?;
            cs.next().is_none().then_some(c)
        };
        match opt.name.as_str() {
            "sep" => match one_char(text) {
                Some(c) if !matches!(c, '"' | '\r' | '\n') => o.sep = c,
                _ => {
                    return err(
                        "`sep` is one character, not a quote or a new line: `sep=\";\"`, `sep=\"\\t\"`"
                            .into(),
                    )
                }
            },
            "decimal" => match one_char(text) {
                Some(c)
                    if !c.is_ascii_digit() && !matches!(c, '-' | '+' | '"' | '\r' | '\n') =>
                {
                    o.decimal = c
                }
                _ => {
                    return err(
                        "`decimal` is the decimal separator, one character: `decimal=\",\"`".into(),
                    )
                }
            },
            "header" => match &opt.value {
                OptionValue::Bool(b) => o.header = *b,
                _ => return err("`header` is `true` or `false`".into()),
            },
            "encoding" => match text.map(|t| t.to_ascii_lowercase()).as_deref() {
                Some("utf-8" | "utf8") => o.encoding = Encoding::Utf8,
                Some("latin1" | "latin-1" | "iso-8859-1") => o.encoding = Encoding::Latin1,
                _ => return err("`encoding` is \"utf-8\" or \"latin1\" (ISO-8859-1)".into()),
            },
            "root" => match text {
                Some(t) if xml_name(t) => o.root = Some(t.to_string()),
                _ => return err("`root` is the name of the root element: `root=\"Invoice\"`, `root=\"p:FatturaElettronica\"`".into()),
            },
            _ => {}
        }
    }
    Ok(o)
}

// ---- places ------------------------------------------------------------------------------

/// Where a field is in a row of CSV.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Column {
    /// the column named so in the header
    Name(String),
    /// the column at this position, from 1 (without a header)
    Position(usize),
}

impl std::fmt::Display for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Column::Name(n) => write!(f, "`{n}`"),
            Column::Position(p) => write!(f, "{p}"),
        }
    }
}

/// The column of each field of a record `from csv`, in the order of `fields`: its name in the
/// header (`at "NAME"`, else the field's name), or without a header its position (`at "3"`,
/// else its order among the fields).
pub fn columns(fields: &[&Field], header: bool) -> Vec<Result<Column, String>> {
    fields
        .iter()
        .enumerate()
        .map(|(i, f)| match (&f.at, header) {
            (Some(a), true) if a.trim().is_empty() => {
                Err("`at` names the column in the header: `at \"PRICE\"`".to_string())
            }
            (Some(a), true) => Ok(Column::Name(a.trim().to_string())),
            (None, true) => Ok(Column::Name(f.name.clone())),
            (Some(a), false) => match a.trim().parse::<usize>() {
                Ok(n) if n >= 1 => Ok(Column::Position(n)),
                _ => Err(format!(
                    "`at \"{a}\"`: without a header a column is its position, from 1: `at \"3\"`"
                )),
            },
            (None, false) => Ok(Column::Position(i + 1)),
        })
        .collect()
}

/// The columns of a field of a record `from fixed`: `at "11..20"` (from 1, both included) or
/// `at "7"` (one column).
pub fn range(at: &str) -> Result<(usize, usize), String> {
    let bad = || {
        format!("`at \"{at}\"`: the columns of the field, from 1, both included: `at \"11..20\"` (or `at \"7\"` for one)")
    };
    let (a, b) = match at.split_once("..") {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (at.trim(), at.trim()),
    };
    let (a, b) = (
        a.parse::<usize>().map_err(|_| bad())?,
        b.parse::<usize>().map_err(|_| bad())?,
    );
    if a < 1 || b < a || b > 100_000 {
        return Err(bad());
    }
    Ok((a, b))
}

/// A step of the path of a field in an XML document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// a child element (its qualified name as written: `cbc:ID`)
    Element(String),
    /// an attribute of the element reached (`@currencyID`): the last step
    Attribute(String),
    /// the text of the element reached (`.`, or the end of a path of elements)
    Text,
}

/// The steps of a field's place in an XML document: `at "a/b"` (elements), `at "a/@id"` (an
/// attribute), `at "."` (the text of the record's own element); without `at`, the child
/// element of the field's name.
pub fn xml_path(f: &Field) -> Result<Vec<Step>, String> {
    let at = match &f.at {
        None => return Ok(vec![Step::Element(f.name.clone()), Step::Text]),
        Some(a) => a.trim(),
    };
    let bad = |why: &str| {
        format!("`at \"{at}\"`: {why} (elements separated by `/`, `@name` for an attribute: `cac:Party/cbc:Name`, `Amount/@currency`, `.`)")
    };
    if at == "." {
        return Ok(vec![Step::Text]);
    }
    let parts: Vec<&str> = at.split('/').collect();
    let mut steps = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if let Some(a) = p.strip_prefix('@') {
            if i + 1 != parts.len() {
                return Err(bad("an attribute is the last step"));
            }
            if !xml_name(a) {
                return Err(bad(&format!("`{a}` is not a name")));
            }
            steps.push(Step::Attribute(a.to_string()));
            return Ok(steps);
        }
        if !xml_name(p) {
            return Err(bad(&format!("`{p}` is not the name of an element")));
        }
        steps.push(Step::Element(p.to_string()));
    }
    steps.push(Step::Text);
    Ok(steps)
}

/// A name of XML, with at most one prefix: `Invoice`, `cbc:ID`, `xml:lang`.
pub fn xml_name(n: &str) -> bool {
    let ok = |s: &str| {
        let mut cs = s.chars();
        cs.next().is_some_and(|c| c.is_alphabetic() || c == '_')
            && cs.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    };
    match n.split_once(':') {
        Some((p, l)) => ok(p) && ok(l),
        None => ok(n),
    }
}

/// The prefix of a qualified name (`cbc` of `cbc:ID`).
pub fn prefix(qname: &str) -> Option<&str> {
    qname.split_once(':').map(|(p, _)| p)
}
