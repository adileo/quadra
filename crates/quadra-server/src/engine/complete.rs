//! `POST /v1/complete`: completion of a formula of an app (a spreadsheet cell, a chart's or a
//! KPI's expression, a `{= … }` of a text) in its context, without touching the data.
//!
//! The context is the one `POST /v1/eval` checks the formula in (engine/sheets.rs): the
//! workbook's function `fn __sheet(…)`, whose parameters are the app's filters and selected
//! records (`@region: [Region]`, typed by what they filter: engine/params.rs) and the cells the
//! formula names (`B2`, `Sales!A1:A9`: typed by their values, or by their own formulas). What
//! is before the caret is read as `/v1/eval` reads it (the same rewriting of `@name` and of
//! cell references, the parser of the language) and typed with the model's checked types
//! (`report.field_types`, the signatures of functions, methods and metrics): `o.customer.`
//! offers a customer's fields, `lines.` the methods of a collection and the fields of its
//! elements, `g.` in `group_by(…).map(g => g.` the group's `key` and `items`, `state is ` the
//! values of the enum.
//!
//! Nothing is evaluated and nothing is checked again per request: the headers and docs of the
//! declarations are kept per model version, what the user may read per user and transaction.
//! Entities the user may not read, and fields hidden from them, are not offered.
//!
//! Request `{text, pos, kind, sheets, sheet, params}`, answer `{from, to, items, signature}`:
//! the HTTP API reference (quadra.run/reference/http-api), "Completion of formulas".

use super::ide::{byte_at, utf16};
use super::params::Param;
use super::sheets::{classify, rewrite, type_text, Addr, Book, Input, Sheets, MAX_ROWS};
use super::*;
use quadra_sema::ide::METHODS;
use quadra_syntax::{ast, SyntaxKind::*, SyntaxNode};
use quadra_types::{Num, Unit, T};
use std::collections::HashSet;

/// The headers and docs of the model's declarations, kept per model version.
pub struct Docs {
    checked: Rc<Checked>,
    /// top-level name → (header, doc)
    decls: HashMap<String, (String, Option<String>)>,
    /// (entity, member) → (header, doc)
    members: HashMap<(String, String), (String, Option<String>)>,
}

/// What a user may read, kept per transaction.
pub struct Perms {
    checked: Rc<Checked>,
    base: u64,
    read: HashSet<String>,
    hidden: HashSet<(String, String)>,
}

/// An item of the list.
struct Item {
    label: String,
    kind: &'static str,
    detail: String,
    doc: Option<String>,
    insert: String,
    /// offered first (the values of the enum a comparison expects)
    first: bool,
}

impl Item {
    fn new(label: impl Into<String>, kind: &'static str, detail: impl Into<String>) -> Item {
        let label = label.into();
        Item {
            insert: label.clone(),
            label,
            kind,
            detail: detail.into(),
            doc: None,
            first: false,
        }
    }

    fn doc(mut self, doc: Option<String>) -> Item {
        self.doc = doc.filter(|d| !d.is_empty());
        self
    }

    fn insert(mut self, insert: impl Into<String>) -> Item {
        self.insert = insert.into();
        self
    }
}

/// Methods of values (the collections' are `quadra_sema::ide::METHODS`): name, signature,
/// what it gives.
const TEXT_METHODS: &[(&str, &str, &str)] = &[
    ("len", "len()", "the number of characters"),
    ("upper", "upper()", "in UPPER CASE"),
    ("lower", "lower()", "in lower case"),
    ("trim", "trim()", "without the spaces around"),
    ("take", "take(n)", "the first `n` characters"),
    ("initials", "initials()", "the first letter of each word"),
    (
        "starts_with",
        "starts_with(text)",
        "the text starts with `text`",
    ),
    ("ends_with", "ends_with(text)", "the text ends with `text`"),
    ("contains", "contains(text)", "`text` is in the text"),
];
const NUMBER_METHODS: &[(&str, &str, &str)] = &[
    (
        "round",
        "round()",
        "rounded (an amount: to its currency's decimals)",
    ),
    ("abs", "abs()", "the absolute value"),
];
const DATE_METHODS: &[(&str, &str, &str)] = &[
    (
        "start_of_month",
        "start_of_month()",
        "the first day of its month",
    ),
    (
        "end_of_month",
        "end_of_month()",
        "the last day of its month",
    ),
];
const RANGE_METHODS: &[(&str, &str, &str)] =
    &[("contains", "contains(date)", "the date is in the range")];
/// Functions of the language that formulas use.
const BUILTINS: &[(&str, &str, &str)] = &[
    ("today", "today()", "today's date"),
    ("now", "now()", "the date and time now"),
    (
        "DateRange",
        "DateRange(from, to)",
        "the dates from `from` to `to`, both included",
    ),
];
const KEYWORDS: &[&str] = &[
    "if", "else", "and", "or", "not", "is", "in", "none", "true", "false", "match", "when",
    "current",
];
const MAX_TEXT: usize = 20_000;
const MAX_ITEMS: usize = 100;

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The part of `text` that is an expression, around the caret (`byte`): (start, end). None:
/// the caret is not in one (a cell holding a value, a text outside `{= … }`).
fn region(text: &str, byte: usize, kind: &str) -> Option<(usize, usize)> {
    match kind {
        "formula" => text
            .starts_with('=')
            .then_some((1, text.len()))
            .filter(|_| byte >= 1),
        "template" => {
            let before = &text[..byte];
            let open = before.rfind("{=")?;
            if before[open..].contains(['}', '\n']) {
                return None;
            }
            let end = text[byte..]
                .find(['}', '\n'])
                .map_or(text.len(), |k| byte + k);
            Some((open + 2, end))
        }
        _ => Some((0, text.len())),
    }
}

/// The text before the caret, read: the brackets still open, and whether the caret is in a
/// text, a comment or a quoted sheet name (`'My sh`).
struct Scan {
    open: Vec<(usize, char)>,
    in_text: bool,
    quote: Option<usize>,
}

fn scan(s: &str) -> Scan {
    let ch: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Scan {
        open: Vec::new(),
        in_text: false,
        quote: None,
    };
    let mut k = 0;
    while k < ch.len() {
        let (i, c) = ch[k];
        let prev = if k > 0 { Some(ch[k - 1].1) } else { None };
        match c {
            '"' => {
                k += 1;
                while k < ch.len() && ch[k].1 != '"' {
                    if ch[k].1 == '\\' {
                        k += 1;
                    }
                    k += 1;
                }
                if k >= ch.len() {
                    out.in_text = true;
                    return out;
                }
            }
            '\'' if prev.is_none_or(|p| !ident_char(p)) => {
                // a sheet's name: `'My sheet'!A1` (`''` is a quote in it)
                let mut j = k + 1;
                loop {
                    if j >= ch.len() {
                        out.quote = Some(i);
                        return out;
                    }
                    if ch[j].1 == '\'' {
                        if j + 1 < ch.len() && ch[j + 1].1 == '\'' {
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
                k = j;
            }
            // `A1#` is the array a formula spilled; any other `#` starts a comment
            '#' if !prev.is_some_and(|p| p.is_ascii_alphanumeric()) => {
                out.in_text = true;
                return out;
            }
            '(' | '[' | '{' => out.open.push((i, c)),
            ')' | ']' | '}' => {
                let want = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if out.open.last().is_some_and(|(_, o)| *o == want) {
                    out.open.pop();
                }
            }
            _ => {}
        }
        k += 1;
    }
    out
}

/// Where the chain of names and calls ending at `end` starts (`o.customer.region`,
/// `Order.where(o => …).first()`, `Sales!B2:B9`, `@region`).
fn chain_start(s: &str, end: usize) -> usize {
    let mut start = end;
    let mut level = 0;
    let mut quoted = false;
    for (i, c) in s[..end].char_indices().rev() {
        if quoted {
            if c == '\'' {
                quoted = false;
            }
            start = i;
            continue;
        }
        match c {
            ')' | ']' => level += 1,
            '(' | '[' => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            _ if level > 0 => {}
            '\'' => quoted = true,
            c if ident_char(c) || matches!(c, '.' | '?' | '@' | '$' | '!' | '#' | ':') => {}
            _ => break,
        }
        start = i;
    }
    start
}

/// The commas at the top level of `s` (an argument list after its `(`): the index of the
/// argument the caret is in.
fn arg_index(s: &str) -> usize {
    let mut n = 0;
    let mut level = 0;
    let mut in_text = false;
    let mut esc = false;
    for c in s.chars() {
        if in_text {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_text = false;
            }
            continue;
        }
        match c {
            '"' => in_text = true,
            '(' | '[' | '{' => level += 1,
            ')' | ']' | '}' => level -= 1,
            ',' if level == 0 => n += 1,
            _ => {}
        }
    }
    n
}

/// `x =>` written in `s` (an argument list after its `(`) at its top level, in the argument the
/// caret is in: the lambda's parameter.
fn lambda_param(s: &str) -> Option<String> {
    let mut level = 0;
    let mut arg_start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => level += 1,
            ')' | ']' | '}' => level -= 1,
            ',' if level == 0 => arg_start = i + 1,
            _ => {}
        }
    }
    let arg = &s[arg_start..];
    // `x =>` or `by=x =>`
    let at = arg.find("=>")?;
    let head = arg[..at].trim_end();
    let name: String = head
        .chars()
        .rev()
        .take_while(|c| ident_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let rest = head[..head.len() - name.len()].trim();
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && (rest.is_empty() || rest.ends_with('='));
    ok.then_some(name)
}

/// `recv.method` → (`recv`, `method`); a name alone → (``, name).
fn split_callee(callee: &str) -> (&str, &str) {
    let mut level = 0;
    for (i, c) in callee.char_indices().rev() {
        match c {
            ')' | ']' => level += 1,
            '(' | '[' => level -= 1,
            '.' if level == 0 => {
                let recv = callee[..i].trim_end_matches('?');
                return (recv, &callee[i + 1..]);
            }
            _ => {}
        }
    }
    ("", callee)
}

fn unopt(t: &T) -> &T {
    t.unopt()
}

/// The elements of a collection, or of an entity's records.
fn elem_of(t: &T) -> Option<T> {
    match unopt(t) {
        T::Coll { elem, .. } => Some((**elem).clone()),
        T::Table(e) => Some(T::Ent(e.clone())),
        _ => None,
    }
}

fn list(t: T) -> T {
    T::list(t)
}

/// A type as the language writes it (`[Region]`, `Money<EUR>?`, `Price<EUR, pcs>`), as the
/// checker sees it.
fn t_of_text(model: &Model, s: &str) -> T {
    let s = s.trim();
    if let Some(inner) = s.strip_suffix('?') {
        return T::opt(t_of_text(model, inner));
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return list(t_of_text(model, inner));
    }
    if let Some(inner) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        return T::Coll {
            elem: Box::new(t_of_text(model, inner)),
            set: true,
            facts: Vec::new(),
        };
    }
    if let Some(inner) = s.strip_prefix("enum(").and_then(|x| x.strip_suffix(')')) {
        let variants = inner
            .split(',')
            .map(|v| v.trim().split('(').next().unwrap_or("").to_string())
            .filter(|v| !v.is_empty())
            .collect();
        return T::Enum {
            name: "enum".into(),
            variants,
        };
    }
    let (name, args) = match s.split_once('<') {
        Some((n, rest)) => (
            n.trim(),
            rest.trim_end_matches('>')
                .split(',')
                .map(|a| a.trim().to_string())
                .collect::<Vec<_>>(),
        ),
        None => (s, Vec::new()),
    };
    let unit = |k: usize| match args.get(k) {
        Some(a) if !a.is_empty() && !a.contains('.') => Unit::Const(a.clone()),
        Some(a) if !a.is_empty() => Unit::Path(a.split('.').map(str::to_string).collect()),
        _ => Unit::Unknown,
    };
    match name {
        "Int" => T::Num(Num::Int),
        "Decimal" => T::Num(Num::Decimal),
        "Percent" => T::Num(Num::Percent),
        "Bool" => T::Bool,
        "Date" => T::Date,
        "DateTime" => T::DateTime,
        "Duration" => T::Duration,
        "DateRange" => T::DateRange,
        "YearMonth" => T::YearMonth,
        "Text" => T::text(),
        "Code" => T::Text("Code"),
        "Email" => T::Text("Email"),
        "Phone" => T::Text("Phone"),
        "Money" => T::Money {
            cur: unit(0),
            exact: false,
        },
        "Qty" => T::Qty {
            unit: unit(0),
            exact: false,
        },
        "Price" => T::Price {
            cur: unit(0),
            per: unit(1),
            exact: false,
        },
        "Exact" => t_of_text(model, args.join(", ").as_str()).with_exact(true),
        n if model.entity(n).is_some() => T::Ent(n.to_string()),
        n => match model.enum_def(n) {
            Some(e) => T::Enum {
                name: e.name.clone(),
                variants: e.variants.iter().map(|v| v.name.clone()).collect(),
            },
            None => T::Unknown,
        },
    }
}

fn t_of_ty(model: &Model, t: &quadra_ir::Ty) -> T {
    t_of_text(model, &quadra_ir::print::ty(t))
}

fn signature(name: &str, params: &[quadra_ir::Param], ret: Option<&quadra_ir::Ty>) -> String {
    let ps: Vec<String> = params
        .iter()
        .map(|p| match &p.ty {
            Some(t) => format!("{}: {}", p.name, quadra_ir::print::ty(t)),
            None => p.name.clone(),
        })
        .collect();
    let ret = ret
        .map(|t| format!(" -> {}", quadra_ir::print::ty(t)))
        .unwrap_or_default();
    format!("{name}({}){ret}", ps.join(", "))
}

/// The names a bare `Q1` means instead of a cell (as `/v1/eval` reads it).
fn taken(model: &Model, n: &str) -> bool {
    model.entity(n).is_some()
        || model.variant(n).is_some()
        || model.function(n).is_some()
        || model.enum_def(n).is_some()
        || model.consts.iter().any(|c| c.name == n)
        || model.metrics.iter().any(|m| m.name == n)
}

/// `__c_0_2_3` → its cell; `__r_…` → its rectangle; `__sp_…` → the cell whose spill it is.
enum Named {
    Cell(Addr),
    Range(usize, u32, u32, u32, u32),
    Spill(Addr),
}

fn named(n: &str) -> Option<Named> {
    let nums = |rest: &str| {
        rest.split('_')
            .map(|x| x.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
    };
    if let Some(rest) = n.strip_prefix("__c_") {
        let v = nums(rest)?;
        return (v.len() == 3).then(|| {
            Named::Cell(Addr {
                s: v[0] as usize,
                r: v[1] as u32,
                c: v[2] as u32,
            })
        });
    }
    if let Some(rest) = n.strip_prefix("__sp_") {
        let v = nums(rest)?;
        return (v.len() == 3).then(|| {
            Named::Spill(Addr {
                s: v[0] as usize,
                r: v[1] as u32,
                c: v[2] as u32,
            })
        });
    }
    if let Some(rest) = n.strip_prefix("__r_") {
        let v = nums(rest)?;
        return (v.len() == 5).then(|| {
            Named::Range(
                v[0] as usize,
                v[1] as u32,
                v[2] as u32,
                v[3] as u32,
                v[4] as u32,
            )
        });
    }
    None
}

/// Types the expressions of a formula, in the workbook's function: the app's parameters, the
/// cells, the lambdas' parameters in scope.
struct Typer<'a, 'm> {
    model: &'m Model,
    report: &'m quadra_types::Report,
    i: &'a mut Interp<'m>,
    params: &'a [Param],
    book: &'a Book,
    locals: Vec<(String, T)>,
    /// cells being typed (a cycle types as unknown)
    visiting: HashSet<Addr>,
}

impl Typer<'_, '_> {
    /// The type of a piece of formula as written (`@name`, cell references), in sheet `here`.
    fn type_text(&mut self, src: &str, here: usize) -> T {
        if src.trim().is_empty() || src.len() > 4000 {
            return T::Unknown;
        }
        let Ok(src) = super::params::rewrite_params(src, self.params) else {
            return T::Unknown;
        };
        let model = self.model;
        let Ok((text, _)) = rewrite(
            &src,
            here,
            &Sheets {
                names: &self.book.names,
            },
            &|n| taken(model, n),
        ) else {
            return T::Unknown;
        };
        match quadra_syntax::parse_expr(&text) {
            Some(node) => self.infer(&node),
            None => T::Unknown,
        }
    }

    fn field(&self, ent: &str, name: &str) -> T {
        let m = self.model;
        let Some(e) = m.entity(ent) else {
            return T::Unknown;
        };
        if let Some(t) = self
            .report
            .field_types
            .get(&(ent.to_string(), name.to_string()))
        {
            return t.clone();
        }
        if let Some(f) = m.find_field(e, name) {
            if let Some(ty) = &f.ty {
                return t_of_ty(m, ty);
            }
            for t in m.all_traits(e) {
                if let Some(x) = self
                    .report
                    .field_types
                    .get(&(t.name.clone(), name.to_string()))
                {
                    return x.clone();
                }
            }
            return T::Unknown;
        }
        match name {
            "id" => T::text(),
            "created_at" | "updated_at" => T::DateTime,
            "created_by" | "updated_by" => T::Ent("User".into()),
            "version" => T::Num(Num::Int),
            _ => T::Unknown,
        }
    }

    /// `base.name` (no call).
    fn member(&self, base: &T, name: &str) -> T {
        match unopt(base) {
            T::Ent(e) => self.field(e, name),
            T::Coll { elem, .. } => match unopt(elem) {
                // a field of the elements: their values together
                T::Ent(e) => match self.field(e, name) {
                    T::Coll { elem, set, facts } => T::Coll { elem, set, facts },
                    T::Unknown => T::Unknown,
                    t => list(t),
                },
                T::Tuple(items) => match items.iter().find(|(n, _)| n.as_deref() == Some(name)) {
                    Some((_, t)) => list(t.clone()),
                    None => T::Unknown,
                },
                _ => T::Unknown,
            },
            T::Tuple(items) => items
                .iter()
                .find(|(n, _)| n.as_deref() == Some(name))
                .map(|(_, t)| t.clone())
                .unwrap_or(T::Unknown),
            T::Date | T::DateTime if name == "month" => T::YearMonth,
            T::Date | T::DateTime if name == "year" => T::Num(Num::Int),
            T::DateTime if name == "date" => T::Date,
            T::Money { .. } if name == "currency" => T::Ent("Currency".into()),
            T::Money { .. } | T::Price { .. } | T::Qty { .. } | T::Num(_) if name == "raw" => {
                T::Num(Num::Decimal)
            }
            T::Current => match name {
                "user" => T::Ent("User".into()),
                "company" => T::Ent("Company".into()),
                _ => T::Unknown,
            },
            _ => T::Unknown,
        }
    }

    /// The type of a lambda's body (`x => body`, `x` an element of the receiver), or of a
    /// plain argument.
    fn arg_body(&mut self, arg: Option<&SyntaxNode>, param_t: &T) -> Option<T> {
        let arg = arg?;
        let e = arg.children().find(|c| c.kind() != NAME)?;
        if e.kind() == LAMBDA_EXPR {
            let names: Vec<String> = ast::child(&e, PARAM_LIST)
                .map(|l| {
                    ast::children(&l, PARAM)
                        .filter_map(|p| ast::name_text(&p))
                        .collect()
                })
                .unwrap_or_default();
            let body = e.children().filter(|c| c.kind() != PARAM_LIST).last()?;
            let n = self.locals.len();
            for p in names {
                self.locals.push((p, param_t.clone()));
            }
            let t = self.infer(&body);
            self.locals.truncate(n);
            return Some(t);
        }
        Some(self.infer(&e))
    }

    /// `base.name(args)`.
    fn method(&mut self, base: &T, name: &str, args: &[SyntaxNode]) -> T {
        let b = unopt(base).clone();
        if let Some(elem) = elem_of(&b) {
            let coll = match &b {
                T::Table(_) => list(elem.clone()),
                t => t.clone(),
            };
            let lit_number = args
                .first()
                .and_then(|a| a.children().next())
                .is_some_and(|e| e.kind() == LITERAL);
            return match name {
                "where" | "filter" | "sort_by" | "distinct" | "all"
                    if !(name == "all" && !args.is_empty()) =>
                {
                    coll
                }
                "all" | "any" | "none" | "contains" | "is_contiguous" => T::Bool,
                "first" | "last" if lit_number => coll,
                "first" | "last" | "find" | "single" | "last_by" | "min_by" | "max_by" => {
                    T::opt(elem)
                }
                "count" => T::Num(Num::Int),
                "map" => list(self.arg_body(args.first(), &elem).unwrap_or(T::Unknown)),
                "flat_map" => match self.arg_body(args.first(), &elem) {
                    Some(t @ T::Coll { .. }) => t,
                    Some(T::Table(e)) => list(T::Ent(e)),
                    Some(t) => list(t),
                    None => list(T::Unknown),
                },
                "sum" | "avg" => self
                    .arg_body(args.first(), &elem)
                    .unwrap_or(elem)
                    .unopt()
                    .clone(),
                "min" | "max" => T::opt(
                    self.arg_body(args.first(), &elem)
                        .unwrap_or(elem)
                        .unopt()
                        .clone(),
                ),
                "group_by" => {
                    let key = self.arg_body(args.first(), &elem).unwrap_or(T::Unknown);
                    let mut items = vec![(Some("key".to_string()), key)];
                    if args.len() > 1 {
                        let key2 = self.arg_body(args.get(1), &elem).unwrap_or(T::Unknown);
                        items.push((Some("key2".to_string()), key2));
                    }
                    items.push((Some("items".to_string()), list(elem)));
                    list(T::Tuple(items))
                }
                _ => T::Unknown,
            };
        }
        match &b {
            T::Text(_) => match name {
                "len" => T::Num(Num::Int),
                "starts_with" | "ends_with" | "contains" => T::Bool,
                "upper" | "lower" | "trim" | "take" | "initials" => T::text(),
                _ => T::Unknown,
            },
            T::Num(_) | T::Money { .. } | T::Qty { .. } | T::Price { .. } => match name {
                "round" => b.clone().with_exact(false),
                "abs" => b.clone(),
                "as_percent" => T::Num(Num::Percent),
                _ => T::Unknown,
            },
            T::Date | T::DateTime if matches!(name, "start_of_month" | "end_of_month") => b.clone(),
            T::DateRange if name == "contains" => T::Bool,
            T::Ent(e) => {
                let Some(ent) = self.model.entity(e) else {
                    return T::Unknown;
                };
                if name == "closure" {
                    return list(T::Ent(e.clone()));
                }
                match ent.methods.iter().find(|f| f.name == name) {
                    Some(f) => f
                        .ret
                        .as_ref()
                        .map_or(T::Unknown, |t| t_of_ty(self.model, t)),
                    None => T::Unknown,
                }
            }
            _ => T::Unknown,
        }
    }

    fn name(&mut self, n: &str) -> T {
        if let Some((_, t)) = self.locals.iter().rev().find(|(x, _)| x == n) {
            return t.clone();
        }
        if let Some(p) = self.params.iter().find(|p| p.var == n) {
            return p
                .ty
                .as_deref()
                .map_or(T::Unknown, |t| t_of_text(self.model, t));
        }
        if let Some(r) = named(n) {
            return self.named(r);
        }
        let m = self.model;
        if let Some(e) = m.entity(n) {
            return if e.kind == EntityKind::Entity {
                T::Table(n.to_string())
            } else {
                T::Unknown
            };
        }
        if let Some(c) = m.constant(n) {
            return c.ty.as_ref().map_or(T::Unknown, |t| t_of_ty(m, t));
        }
        match n {
            "current" => T::Current,
            "me" => T::Ent("User".into()),
            _ if m.variant(n).is_some() => T::Variant(n.to_string()),
            _ => T::Unknown,
        }
    }

    /// A cell's value: typed by the value it holds, or by its formula.
    fn cell(&mut self, a: Addr) -> T {
        let Some(raw) = self
            .book
            .cells
            .get(a.s)
            .and_then(|c| c.get(&(a.r, a.c)))
            .cloned()
        else {
            return T::Unknown;
        };
        match classify(self.i, &raw) {
            Some(Input::Value(v)) => {
                type_text(&v).map_or(T::Unknown, |t| t_of_text(self.model, &t))
            }
            Some(Input::Formula(f)) => {
                if self.visiting.len() > 8 || !self.visiting.insert(a) {
                    return T::Unknown;
                }
                // the formula's own lambdas are its own
                let outer = std::mem::take(&mut self.locals);
                let t = self.type_text(&f, a.s);
                self.locals = outer;
                self.visiting.remove(&a);
                t
            }
            None => T::Unknown,
        }
    }

    fn named(&mut self, r: Named) -> T {
        match r {
            Named::Cell(a) | Named::Spill(a) => self.cell(a),
            Named::Range(s, r1, c1, r2, c2) => {
                // the type of its first cell holding something
                let first = self.book.cells.get(s).and_then(|cs| {
                    cs.range((r1, 0)..=(r2.min(MAX_ROWS), u32::MAX))
                        .find(|((_, c), _)| *c >= c1 && *c <= c2)
                        .map(|((r, c), _)| (*r, *c))
                });
                match first {
                    Some((r, c)) => list(self.cell(Addr { s, r, c }).unopt().clone()),
                    None => list(T::Unknown),
                }
            }
        }
    }

    fn infer(&mut self, n: &SyntaxNode) -> T {
        let exprs: Vec<SyntaxNode> = n
            .children()
            .filter(|c| !matches!(c.kind(), ARG_LIST | NAME | PARAM_LIST | UNIT))
            .collect();
        match n.kind() {
            NAME_REF => {
                let t = ast::token(n, IDENT)
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                self.name(&t)
            }
            LITERAL => match n
                .children_with_tokens()
                .filter_map(|t| t.into_token())
                .find(|t| !t.kind().is_trivia())
                .map(|t| t.kind())
            {
                Some(STRING | FSTRING) => T::text(),
                Some(NUMBER) => {
                    if n.text().to_string().contains('.') {
                        T::Num(Num::Decimal)
                    } else {
                        T::Num(Num::Int)
                    }
                }
                Some(PERCENT) => T::Num(Num::Percent),
                Some(DATE) => T::Date,
                Some(DATETIME) => T::DateTime,
                Some(YEAR_MONTH) => T::YearMonth,
                Some(TRUE_KW | FALSE_KW) => T::Bool,
                Some(NONE_KW) => T::None,
                _ => T::Unknown,
            },
            QUANTITY => {
                let units: Vec<String> = ast::child(n, UNIT)
                    .map(|u| {
                        u.children_with_tokens()
                            .filter_map(|t| t.into_token())
                            .filter(|t| t.kind() == IDENT)
                            .map(|t| t.text().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let u = units.first().cloned().unwrap_or_default();
                let currency = u.len() == 3 && u.chars().all(|c| c.is_ascii_uppercase());
                if quadra_types::duration_unit(&u).is_some() {
                    T::Duration
                } else if let Some(per) = units.get(1) {
                    T::Price {
                        cur: Unit::Const(u),
                        per: Unit::Const(per.clone()),
                        exact: false,
                    }
                } else if currency {
                    T::Money {
                        cur: Unit::Const(u),
                        exact: false,
                    }
                } else {
                    T::Qty {
                        unit: Unit::Const(u),
                        exact: false,
                    }
                }
            }
            PAREN_EXPR => exprs.first().map_or(T::Unknown, |e| self.infer(e)),
            FIELD_EXPR => {
                let Some(base) = exprs.first() else {
                    return T::Unknown;
                };
                let name = n
                    .children()
                    .filter(|c| c.kind() == NAME_REF)
                    .last()
                    .and_then(|c| ast::token(&c, IDENT))
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let bt = self.infer(base);
                self.member(&bt, &name)
            }
            CALL_EXPR => {
                let args: Vec<SyntaxNode> = ast::child(n, ARG_LIST)
                    .map(|l| ast::children(&l, ARG).collect())
                    .unwrap_or_default();
                let Some(callee) = exprs.first() else {
                    return T::Unknown;
                };
                match callee.kind() {
                    FIELD_EXPR => {
                        let parts: Vec<SyntaxNode> = callee
                            .children()
                            .filter(|c| !matches!(c.kind(), NAME))
                            .collect();
                        let Some(base) = parts.first() else {
                            return T::Unknown;
                        };
                        let name = callee
                            .children()
                            .filter(|c| c.kind() == NAME_REF)
                            .last()
                            .and_then(|c| ast::token(&c, IDENT))
                            .map(|t| t.text().to_string())
                            .unwrap_or_default();
                        let bt = self.infer(base);
                        self.method(&bt, &name, &args)
                    }
                    NAME_REF => {
                        let name = ast::token(callee, IDENT)
                            .map(|t| t.text().to_string())
                            .unwrap_or_default();
                        let m = self.model;
                        if let Some(f) = m.function(&name) {
                            return f.ret.as_ref().map_or(T::Unknown, |t| t_of_ty(m, t));
                        }
                        if let Some(mt) = m.metrics.iter().find(|x| x.name == name) {
                            return mt.ret.as_ref().map_or(T::Unknown, |t| t_of_ty(m, t));
                        }
                        match name.as_str() {
                            "today" => T::Date,
                            "now" => T::DateTime,
                            "DateRange" => T::DateRange,
                            n if m.entity(n).is_some() => T::Ent(n.to_string()),
                            _ => T::Unknown,
                        }
                    }
                    _ => T::Unknown,
                }
            }
            INDEX_EXPR => exprs
                .first()
                .map(|e| self.infer(e))
                .and_then(|t| elem_of(&t))
                .unwrap_or(T::Unknown),
            PREFIX_EXPR => {
                if ast::token(n, NOT_KW).is_some() {
                    T::Bool
                } else {
                    exprs.first().map_or(T::Unknown, |e| self.infer(e))
                }
            }
            BIN_EXPR => {
                let ops: Vec<quadra_syntax::SyntaxKind> = n
                    .children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .map(|t| t.kind())
                    .filter(|k| !k.is_trivia())
                    .collect();
                let has = |k| ops.contains(&k);
                if has(IS_KW)
                    || has(IN_KW)
                    || has(AND_KW)
                    || has(OR_KW)
                    || has(EQ2)
                    || has(NEQ)
                    || has(LT)
                    || has(LE)
                    || has(GT)
                    || has(GE)
                    || has(NOT_KW)
                {
                    return T::Bool;
                }
                if has(QUESTION2) {
                    let l = exprs.first().map_or(T::Unknown, |e| self.infer(e));
                    return unopt(&l).clone();
                }
                if has(DOT2) {
                    return T::Unknown;
                }
                let l = exprs.first().map_or(T::Unknown, |e| self.infer(e));
                let r = exprs.get(1).map_or(T::Unknown, |e| self.infer(e));
                match (unopt(&l), unopt(&r)) {
                    (T::Date | T::DateTime, T::Duration) => unopt(&l).clone(),
                    (T::Date, T::Date) if has(MINUS) => T::Duration,
                    (T::Num(_), x @ (T::Money { .. } | T::Qty { .. } | T::Price { .. }))
                        if has(STAR) =>
                    {
                        x.clone()
                    }
                    (T::Qty { .. }, p @ T::Price { cur, .. }) if has(STAR) => {
                        let _ = p;
                        T::Money {
                            cur: cur.clone(),
                            exact: true,
                        }
                    }
                    (T::Money { .. } | T::Qty { .. }, T::Money { .. } | T::Qty { .. })
                        if has(SLASH) =>
                    {
                        T::Num(Num::Decimal)
                    }
                    (x, _) => x.clone(),
                }
            }
            IF_EXPR => exprs.first().map_or(T::Unknown, |e| self.infer(e)),
            TUPLE_EXPR => {
                let mut items = Vec::new();
                for c in n.children() {
                    if c.kind() == NAMED_ITEM {
                        let name = ast::name_text(&c);
                        let v = c
                            .children()
                            .filter(|x| x.kind() != NAME)
                            .last()
                            .map_or(T::Unknown, |e| self.infer(&e));
                        items.push((name, v));
                    } else if c.kind() != NAME {
                        items.push((None, self.infer(&c)));
                    }
                }
                T::Tuple(items)
            }
            LIST_EXPR => {
                let first = n.children().next();
                let t = match first {
                    Some(a) if a.kind() == ARG => a
                        .children()
                        .find(|c| c.kind() != NAME)
                        .map_or(T::Unknown, |e| self.infer(&e)),
                    Some(e) => self.infer(&e),
                    None => T::Unknown,
                };
                list(t)
            }
            SET_EXPR => {
                let t = exprs.first().map_or(T::Unknown, |e| self.infer(e));
                T::Coll {
                    elem: Box::new(t),
                    set: true,
                    facts: Vec::new(),
                }
            }
            _ => T::Unknown,
        }
    }
}

/// The items after `.` on a value of type `t`.
fn members_of(t: &T, typer: &Typer, perms: &Perms, docs: &Docs, out: &mut Vec<Item>) {
    let model = typer.model;
    let fields = |e: &str, together: bool, out: &mut Vec<Item>| {
        if !perms.read.contains(e) {
            return;
        }
        let Some(ent) = model.entity(e) else { return };
        for f in model.all_fields(ent) {
            if f.name.starts_with("__") || perms.hidden.contains(&(e.to_string(), f.name.clone())) {
                continue;
            }
            let ft = typer.field(e, &f.name);
            let shown = match (&ft, together) {
                (T::Unknown, _) => String::new(),
                (T::Coll { .. }, true) | (_, false) => ft.to_string(),
                (_, true) => format!("[{ft}]"),
            };
            let doc = docs
                .members
                .get(&(e.to_string(), f.name.clone()))
                .or_else(|| {
                    model
                        .all_traits(ent)
                        .iter()
                        .find_map(|t| docs.members.get(&(t.name.clone(), f.name.clone())))
                });
            let doc = doc.map(|(h, d)| match d {
                Some(d) => format!("`{h}`\n\n{d}"),
                None => format!("`{h}`"),
            });
            out.push(
                Item::new(
                    f.name.clone(),
                    if f.formula.is_some() {
                        "formula"
                    } else {
                        "field"
                    },
                    shown,
                )
                .doc(doc),
            );
        }
        if !together {
            for m in &ent.methods {
                let sig = signature(&m.name, &m.params, m.ret.as_ref());
                let doc = docs
                    .members
                    .get(&(e.to_string(), m.name.clone()))
                    .and_then(|(_, d)| d.clone());
                out.push(
                    Item::new(m.name.clone(), "method", sig)
                        .doc(doc)
                        .insert(format!("{}(", m.name)),
                );
            }
            for (n, t) in [
                ("id", "Text"),
                ("created_at", "DateTime"),
                ("updated_at", "DateTime"),
                ("created_by", "User"),
                ("updated_by", "User"),
                ("version", "Int"),
            ] {
                if !out.iter().any(|i| i.label == n) {
                    out.push(
                        Item::new(n, "field", t)
                            .doc(Some("kept by Quadra for every record".into())),
                    );
                }
            }
        }
    };
    let methods = |list: &[(&str, &str, &str)], out: &mut Vec<Item>| {
        for (n, sig, doc) in list {
            let no_args = sig.split(" · ").all(|s| s.ends_with("()"));
            out.push(
                Item::new(*n, "method", format!(".{sig}"))
                    .doc(Some(doc.to_string()))
                    .insert(if no_args {
                        format!("{n}()")
                    } else {
                        format!("{n}(")
                    }),
            );
        }
    };
    let value_field = |n: &str, ty: &str, doc: &str, out: &mut Vec<Item>| {
        out.push(Item::new(n, "field", ty).doc(Some(doc.to_string())))
    };
    match unopt(t) {
        T::Ent(e) => fields(e, false, out),
        T::Table(e) => {
            if perms.read.contains(e) {
                methods(METHODS, out);
            }
        }
        T::Coll { elem, .. } => {
            methods(METHODS, out);
            match unopt(elem) {
                T::Ent(e) => fields(e, true, out),
                T::Tuple(items) => {
                    for (n, it) in items {
                        if let Some(n) = n {
                            out.push(Item::new(n.clone(), "field", format!("[{it}]")));
                        }
                    }
                }
                _ => {}
            }
        }
        T::Tuple(items) => {
            for (n, it) in items {
                if let Some(n) = n {
                    out.push(Item::new(n.clone(), "field", it.to_string()));
                }
            }
        }
        T::Text(_) => methods(TEXT_METHODS, out),
        T::Num(k) => {
            methods(NUMBER_METHODS, out);
            if *k != Num::Percent {
                out.push(
                    Item::new("as_percent", "method", ".as_percent()")
                        .doc(Some("the number as a percentage (0.25 → 25%)".into()))
                        .insert("as_percent()"),
                );
            }
        }
        T::Money { .. } => {
            methods(NUMBER_METHODS, out);
            value_field("currency", "Currency", "the amount's currency", out);
            value_field("raw", "Decimal", "the number, without its currency", out);
        }
        T::Qty { .. } | T::Price { .. } => {
            methods(NUMBER_METHODS, out);
            value_field("raw", "Decimal", "the number, without its unit", out);
        }
        T::Date | T::DateTime => {
            methods(DATE_METHODS, out);
            value_field("month", "YearMonth", "its year and month", out);
            value_field("year", "Int", "its year", out);
            if matches!(unopt(t), T::DateTime) {
                value_field("date", "Date", "its day", out);
            }
        }
        T::DateRange => methods(RANGE_METHODS, out),
        T::Current => {
            value_field("user", "User", "the user looking at the app", out);
            value_field("company", "Company", "the current company", out);
        }
        _ => {}
    }
}

/// The signature of a method of a value of type `t`: (label, doc).
fn method_signature(t: &T, name: &str, model: &Model, docs: &Docs) -> Option<(String, String)> {
    let find = |list: &[(&str, &str, &str)]| {
        list.iter()
            .find(|m| m.0 == name)
            .map(|(_, s, d)| (s.to_string(), d.to_string()))
    };
    match unopt(t) {
        T::Table(_) | T::Coll { .. } => find(METHODS),
        T::Text(_) => find(TEXT_METHODS),
        T::Num(_) | T::Money { .. } | T::Qty { .. } | T::Price { .. } => find(NUMBER_METHODS),
        T::Date | T::DateTime => find(DATE_METHODS),
        T::DateRange => find(RANGE_METHODS),
        T::Ent(e) => {
            let ent = model.entity(e)?;
            let m = ent.methods.iter().find(|f| f.name == name)?;
            let doc = docs
                .members
                .get(&(e.clone(), name.to_string()))
                .and_then(|(_, d)| d.clone())
                .unwrap_or_default();
            Some((signature(&m.name, &m.params, m.ret.as_ref()), doc))
        }
        _ => None,
    }
}

/// `sum() · sum(x => v)` → the form with an argument at `active` (else the longest), and its
/// parameters.
fn pick_form(sig: &str, active: usize) -> (String, Vec<String>) {
    let forms: Vec<(String, Vec<String>)> = sig
        .split(" · ")
        .map(|f| {
            let f = f.trim().trim_start_matches('.');
            let inner = f
                .find('(')
                .and_then(|a| f.rfind(')').map(|b| &f[a + 1..b]))
                .unwrap_or("");
            let mut params = Vec::new();
            let mut level = 0;
            let mut cur = String::new();
            for c in inner.chars() {
                match c {
                    '(' | '[' | '<' => level += 1,
                    ')' | ']' | '>' if level > 0 && !cur.ends_with('=') => level -= 1,
                    ',' if level == 0 => {
                        params.push(std::mem::take(&mut cur).trim().to_string());
                        continue;
                    }
                    _ => {}
                }
                cur.push(c);
            }
            if !cur.trim().is_empty() {
                params.push(cur.trim().to_string());
            }
            (f.to_string(), params)
        })
        .collect();
    forms
        .iter()
        .find(|(_, p)| p.len() > active)
        .or_else(|| forms.iter().max_by_key(|(_, p)| p.len()))
        .cloned()
        .unwrap_or_default()
}

fn metric_doc(m: &quadra_ir::Metric) -> String {
    let get = |k: &str| {
        m.meta
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, e)| match &e.kind {
                quadra_ir::ExprKind::Lit(quadra_ir::Lit::Str(s)) => Some(s.trim().to_string()),
                _ => None,
            })
    };
    let mut parts = Vec::new();
    if let Some(l) = get("label") {
        parts.push(format!("**{l}**"));
    }
    if let Some(d) = get("definition") {
        parts.push(d.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    parts.push("a certified metric".into());
    parts.join("\n\n")
}

impl Engine {
    /// The headers and docs of the declarations of the model in use (one analysis per version).
    fn complete_docs(&mut self) -> Rc<Docs> {
        if let Some(d) = &self.sheets.complete_docs {
            if Rc::ptr_eq(&d.checked, &self.checked) {
                return d.clone();
            }
        }
        let analysis = Project::from_sources(&self.root, self.files.clone()).analyze();
        let mut decls = HashMap::new();
        let mut members = HashMap::new();
        for (k, d) in analysis.decls.iter().enumerate() {
            decls
                .entry(d.name.clone())
                .or_insert((d.header.clone(), d.doc.clone()));
            if let Some(ms) = analysis.members.get(&quadra_sema::DeclId(k as u32)) {
                for m in ms {
                    members
                        .entry((d.name.clone(), m.name.clone()))
                        .or_insert((m.header.clone(), m.doc.clone()));
                }
            }
        }
        let docs = Rc::new(Docs {
            checked: self.checked.clone(),
            decls,
            members,
        });
        self.sheets.complete_docs = Some(docs.clone());
        docs
    }

    /// What the user may read (entities) and not see (fields), as of the transaction `i` reads.
    fn complete_perms(&mut self, i: &mut Interp, model: &Model, who: &Principal) -> Rc<Perms> {
        let base = i.store.base;
        if let Some(p) = self.sheets.complete_perms.get(&who.user_id) {
            if p.base == base && Rc::ptr_eq(&p.checked, &self.checked) {
                return p.clone();
            }
        }
        let mut read = HashSet::new();
        let mut hidden = HashSet::new();
        for e in &model.entities {
            if e.name.starts_with("__") || !i.can("read", &e.name, None, None) {
                continue;
            }
            read.insert(e.name.clone());
            for f in model.all_fields(e) {
                if i.hidden(&e.name, &f.name) {
                    hidden.insert((e.name.clone(), f.name.clone()));
                }
            }
        }
        let p = Rc::new(Perms {
            checked: self.checked.clone(),
            base,
            read,
            hidden,
        });
        if self.sheets.complete_perms.len() > 1000 {
            self.sheets.complete_perms.clear();
        }
        self.sheets.complete_perms.insert(who.user_id, p.clone());
        p
    }

    pub(super) fn formula_complete(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        let started = std::time::Instant::now();
        let text = body["text"]
            .as_str()
            .ok_or_else(|| (400, "`text`: the formula, as typed".to_string()))?;
        if text.len() > MAX_TEXT {
            return Err((400, format!("`text`: at most {MAX_TEXT} characters")));
        }
        let kind = body["kind"].as_str().unwrap_or("formula");
        if !matches!(kind, "formula" | "expr" | "template") {
            return Err((400, "`kind`: formula, expr or template".into()));
        }
        let pos = match &body["pos"] {
            J::Null => utf16(text, text.len()),
            p => p
                .as_u64()
                .ok_or_else(|| (400, "`pos`: the caret, in UTF-16 code units".to_string()))?
                as usize,
        };
        let byte = byte_at(text, pos);
        let empty = |from: usize| {
            reply(
                200,
                json!({ "from": utf16(text, from), "to": utf16(text, from), "items": [], "signature": null }),
            )
        };
        let Some((start, end)) = region(text, byte, kind) else {
            return Ok(empty(byte));
        };
        // the workbook: sheets with their cells (as `/v1/eval` reads them), or just their names
        let sheets: Vec<J> = body["sheets"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| {
                        if let Some(n) = s.as_str() {
                            json!({ "name": n })
                        } else {
                            s.clone()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let book = Book::from_json(&json!({ "sheets": sheets }))?;
        let here = body["sheet"]
            .as_str()
            .and_then(|s| {
                book.ids
                    .iter()
                    .position(|x| x == s)
                    .or_else(|| book.names.iter().position(|x| x.eq_ignore_ascii_case(s)))
            })
            .unwrap_or(0);
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let docs = self.complete_docs();
        let mut i = self.interp(model, Some(who), None)?;
        let perms = self.complete_perms(&mut i, model, who);
        // the app's parameters, typed by what they filter (their values do not matter here)
        let stripped: Vec<J> = body["params"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| {
                        let mut p = p.clone();
                        if let Some(o) = p.as_object_mut() {
                            o.remove("value");
                        }
                        p
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (params, params_error) =
            match super::params::params_of(&mut i, model, &json!({ "params": stripped })) {
                Ok(p) => (p.list, None),
                Err((_, m)) => (Vec::new(), Some(m)),
            };
        let mut typer = Typer {
            model,
            report: &checked.report,
            i: &mut i,
            params: &params,
            book: &book,
            locals: Vec::new(),
            visiting: HashSet::new(),
        };

        let src = &text[start..end];
        let at = byte - start;
        let before = &src[..at];
        let sc = scan(before);
        let after_ident = src[at..]
            .chars()
            .take_while(|c| ident_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let prefix_len = before
            .chars()
            .rev()
            .take_while(|c| ident_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let pstart = at - prefix_len;
        let prefix = &before[pstart..];
        let mut from = start + pstart;
        let to = start + at + after_ident;
        let mut items: Vec<Item> = Vec::new();
        let mut receiver: Option<String> = None;
        let mut filter = prefix.to_string();

        // the lambdas whose bodies the caret is in: their parameters, typed
        for (p, c) in sc.open.clone() {
            if c != '(' {
                continue;
            }
            let Some(param) = lambda_param(&before[p + 1..]) else {
                continue;
            };
            let cs = chain_start(before, p);
            let (recv, method) = split_callee(&before[cs..p]);
            let t = if recv.is_empty() {
                T::Unknown
            } else {
                let rt = typer.type_text(recv, here);
                match (unopt(&rt), method) {
                    (T::Ent(e), "closure") => T::Ent(e.clone()),
                    _ => elem_of(&rt).unwrap_or(T::Unknown),
                }
            };
            typer.locals.push((param, t));
        }

        if sc.in_text {
            // in a text or a comment: nothing
        } else if let Some(q) = sc.quote {
            // `'My sh`: the sheets
            from = start + q;
            filter = before[q + 1..].to_string();
            for n in &book.names {
                items.push(
                    Item::new(n.clone(), "sheet", "sheet")
                        .insert(format!("'{}'!", n.replace('\'', "''"))),
                );
            }
        } else if before[..pstart].ends_with('@') {
            from -= 1;
            for p in params.iter().filter(|p| !p.name.ends_with("__on")) {
                items.push(param_item(p, body));
            }
        } else if before[..pstart].ends_with('.') && !before[..pstart].ends_with("..") {
            // after a dot: what the value before it has
            let dot = pstart - 1;
            let recv_end = if before[..dot].ends_with('?') {
                dot - 1
            } else {
                dot
            };
            let cs = chain_start(before, recv_end);
            let chain = &before[cs..recv_end];
            if !chain.is_empty() && !chain.chars().all(|c| c.is_ascii_digit()) {
                if let Some(e) = model.enum_def(chain) {
                    receiver = Some(e.name.clone());
                    for v in &e.variants {
                        items.push(Item::new(v.name.clone(), "variant", e.name.clone()));
                    }
                } else {
                    let t = typer.type_text(chain, here);
                    if !matches!(t, T::Unknown) {
                        receiver = Some(t.to_string());
                    }
                    members_of(&t, &typer, &perms, &docs, &mut items);
                }
            }
        } else if before[..pstart].ends_with('!') {
            // `Sales!`: a cell's address follows
        } else {
            // a name: the values a comparison expects first, then everything in scope
            for v in expected_variants(before, pstart, &sc, &mut typer, here) {
                let mut it = Item::new(v.0, "variant", v.1);
                it.first = true;
                items.push(it);
            }
            for (n, t) in typer.locals.iter().rev() {
                items.push(
                    Item::new(
                        n.clone(),
                        "variable",
                        if matches!(t, T::Unknown) {
                            String::new()
                        } else {
                            t.to_string()
                        },
                    )
                    .doc(Some("lambda parameter".into())),
                );
            }
            for p in params.iter().filter(|p| !p.name.ends_with("__on")) {
                items.push(param_item(p, body));
            }
            for n in &book.names {
                let insert = if n.chars().all(ident_char)
                    && n.chars().next().is_some_and(|c| c.is_alphabetic())
                {
                    format!("{n}!")
                } else {
                    format!("'{}'!", n.replace('\'', "''"))
                };
                let doc = format!("a cell of this sheet: `{insert}A1`, a range: `{insert}A1:B9`");
                items.push(
                    Item::new(n.clone(), "sheet", "sheet")
                        .insert(insert)
                        .doc(Some(doc)),
                );
            }
            for e in &model.entities {
                if e.kind != EntityKind::Entity
                    || e.name.starts_with("__")
                    || !perms.read.contains(&e.name)
                {
                    continue;
                }
                let doc = docs.decls.get(&e.name).and_then(|(_, d)| d.clone());
                items.push(
                    Item::new(
                        e.name.clone(),
                        "entity",
                        if e.is_ledger { "ledger" } else { "entity" },
                    )
                    .doc(doc),
                );
            }
            for e in &model.enums {
                let doc = docs.decls.get(&e.name).and_then(|(_, d)| d.clone());
                items.push(Item::new(e.name.clone(), "enum", "enum").doc(doc));
                for v in &e.variants {
                    items.push(Item::new(v.name.clone(), "variant", e.name.clone()));
                }
            }
            for f in model
                .fns
                .iter()
                .filter(|f| !f.builtin && !f.name.starts_with("__"))
            {
                let doc = docs.decls.get(&f.name).and_then(|(_, d)| d.clone());
                items.push(
                    Item::new(
                        f.name.clone(),
                        "function",
                        signature(&f.name, &f.params, f.ret.as_ref()),
                    )
                    .doc(doc)
                    .insert(format!("{}(", f.name)),
                );
            }
            for m in model.metrics.iter().filter(|m| !m.control) {
                items.push(
                    Item::new(
                        m.name.clone(),
                        "metric",
                        signature(&m.name, &m.params, m.ret.as_ref()),
                    )
                    .doc(Some(metric_doc(m)))
                    .insert(format!("{}(", m.name)),
                );
            }
            for c in &model.consts {
                let t = c.ty.as_ref().map(quadra_ir::print::ty).unwrap_or_default();
                items.push(
                    Item::new(c.name.clone(), "constant", t)
                        .doc(docs.decls.get(&c.name).and_then(|(_, d)| d.clone())),
                );
            }
            for (n, sig, doc) in BUILTINS {
                items.push(
                    Item::new(*n, "function", *sig)
                        .doc(Some(doc.to_string()))
                        .insert(if sig.ends_with("()") {
                            format!("{n}()")
                        } else {
                            format!("{n}(")
                        }),
                );
            }
            for k in KEYWORDS {
                items.push(Item::new(*k, "keyword", ""));
            }
        }

        // signature help: the call whose arguments the caret is in
        let signature = if sc.in_text {
            None
        } else {
            self_signature(before, &sc, &mut typer, here, model, &docs)
        };

        // the typed prefix first, then names with a part starting with it, then containing it
        let p = filter.to_lowercase();
        let mut seen = HashSet::new();
        let mut scored: Vec<(u8, u8, usize, Item)> = items
            .into_iter()
            .filter(|it| seen.insert((it.label.clone(), it.kind)))
            .enumerate()
            .filter_map(|(k, it)| {
                let l = it.label.trim_start_matches('@').to_lowercase();
                let score = if p.is_empty() || l.starts_with(&p) {
                    0
                } else if l.split('_').skip(1).any(|w| w.starts_with(&p)) {
                    1
                } else if l.contains(&p) {
                    2
                } else {
                    return None;
                };
                // what was typed exactly is not offered again alone
                let rank = if it.first {
                    0
                } else {
                    match it.kind {
                        "variable" | "param" => 1,
                        "field" | "formula" => 2,
                        "method" => 3,
                        "sheet" | "entity" | "metric" | "function" => 4,
                        "variant" | "enum" | "constant" => 5,
                        _ => 6,
                    }
                };
                Some((score, rank, k, it))
            })
            .collect();
        scored.sort_by_key(|a| (a.0, a.1, a.2));
        let exact_only = scored.len() == 1
            && scored[0].3.label.trim_start_matches('@') == prefix
            && !prefix.is_empty();
        let list: Vec<J> = if exact_only {
            Vec::new()
        } else {
            scored.into_iter().take(MAX_ITEMS).map(|(_, _, _, it)| json!({ "label": it.label, "kind": it.kind, "detail": it.detail, "doc": it.doc, "insert": it.insert })).collect()
        };
        let mut out = json!({
            "from": utf16(text, from),
            "to": utf16(text, to),
            "items": list,
            "signature": signature,
            "ms": started.elapsed().as_secs_f64() * 1000.0,
        });
        if let Some(r) = receiver {
            out["receiver"] = json!(r);
        }
        if let Some(e) = params_error {
            out["warning"] = json!(e);
        }
        Ok(reply(200, out))
    }
}

/// An `@name` of the app: its type and what it filters.
fn param_item(p: &Param, body: &J) -> Item {
    // the request's definition: `{name, entity, field, widget}` (`_min`/`_max` of a range)
    let base = p.name.trim_end_matches("_min").trim_end_matches("_max");
    let def = body["params"].as_array().and_then(|a| {
        a.iter()
            .find(|d| d["name"] == json!(p.name) || d["name"] == json!(base))
    });
    let doc = def.map(|d| {
        let entity = d["entity"].as_str().unwrap_or("");
        let what = match (
            d["widget"].as_str().unwrap_or("select"),
            d["field"].as_str(),
        ) {
            ("record", _) => {
                format!("the {entity} selected in a view of the app (none: no row selected)")
            }
            (w, Some(f)) => format!("the app's {w} filter on {entity}.{f}"),
            (w, None) => format!("the app's {w} filter on {entity}"),
        };
        match d["label"].as_str().filter(|l| !l.is_empty() && *l != base) {
            Some(l) => format!("**{l}**: {what}"),
            None => what,
        }
    });
    Item::new(
        format!("@{}", p.name),
        "param",
        p.ty.clone().unwrap_or_default(),
    )
    .doc(doc)
}

/// The values of the enum a comparison before the caret expects: `o.state is `, `== `, `!= `,
/// `in {Confirmed, `.
fn expected_variants(
    before: &str,
    pstart: usize,
    sc: &Scan,
    typer: &mut Typer,
    here: usize,
) -> Vec<(String, String)> {
    let head = before[..pstart].trim_end();
    let lhs_end = if let Some(h) = head
        .strip_suffix(" is not")
        .or_else(|| head.strip_suffix(" is"))
        .or_else(|| head.strip_suffix("=="))
        .or_else(|| head.strip_suffix("!="))
    {
        Some(h.trim_end().len())
    } else if let Some(&(b, '{')) = sc.open.last() {
        let h = before[..b].trim_end();
        h.strip_suffix(" in")
            .or_else(|| h.strip_suffix(" not in"))
            .map(|x| x.trim_end().len())
    } else {
        None
    };
    let Some(end) = lhs_end else {
        return Vec::new();
    };
    let cs = chain_start(before, end);
    if cs == end {
        return Vec::new();
    }
    let t = typer.type_text(&before[cs..end], here);
    match unopt(&t) {
        T::Enum { name, variants } => variants.iter().map(|v| (v.clone(), name.clone())).collect(),
        _ => Vec::new(),
    }
}

/// The signature of the call whose arguments the caret is in: `{label, params, active, doc}`.
fn self_signature(
    before: &str,
    sc: &Scan,
    typer: &mut Typer,
    here: usize,
    model: &Model,
    docs: &Docs,
) -> Option<J> {
    // the innermost call (a `(` grouping an expression is not one)
    let (p, callee) = sc
        .open
        .iter()
        .rev()
        .filter(|(_, c)| *c == '(')
        .map(|&(p, _)| (p, &before[chain_start(before, p)..p]))
        .find(|(_, callee)| !callee.is_empty())?;
    let active = arg_index(&before[p + 1..]);
    let (recv, name) = split_callee(callee);
    let (sig, doc) = if recv.is_empty() {
        if let Some(f) = model.function(name).filter(|f| !f.builtin) {
            (
                signature(&f.name, &f.params, f.ret.as_ref()),
                docs.decls
                    .get(name)
                    .and_then(|(_, d)| d.clone())
                    .unwrap_or_default(),
            )
        } else if let Some(m) = model.metrics.iter().find(|m| m.name == name) {
            (signature(&m.name, &m.params, m.ret.as_ref()), metric_doc(m))
        } else if let Some((_, s, d)) = BUILTINS.iter().find(|b| b.0 == name) {
            (s.to_string(), d.to_string())
        } else {
            return None;
        }
    } else {
        let t = typer.type_text(recv, here);
        method_signature(&t, name, model, docs)?
    };
    let (label, params) = pick_form(&sig, active);
    Some(json!({ "label": label, "params": params, "active": active, "doc": doc }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_the_text() {
        assert_eq!(region("=A1+", 4, "formula"), Some((1, 4)));
        assert_eq!(region("12", 1, "formula"), None);
        assert_eq!(region("x {= Order.c } y", 12, "template"), Some((4, 13)));
        assert_eq!(region("x {= a } y", 10, "template"), None);
        let s = scan("Order.where(o => o.name == \"a(\" and o.");
        assert_eq!(s.open.len(), 1);
        assert!(!s.in_text);
        assert!(scan("\"abc").in_text);
        assert!(scan("A1 + 2 # note").in_text);
        assert!(!scan("A1# + 1").in_text);
        assert_eq!(scan("'My sh").quote, Some(0));
        assert_eq!(scan("'My sheet'!A1 + 1").quote, None);
        let t = "x + Order.where(o => o.total > 3).first().customer";
        assert_eq!(
            &t[chain_start(t, t.len())..],
            "Order.where(o => o.total > 3).first().customer"
        );
        let t = "1 + 'My sheet'!B2:B9";
        assert_eq!(&t[chain_start(t, t.len())..], "'My sheet'!B2:B9");
        assert_eq!(lambda_param("o => o.total > 1 and o."), Some("o".into()));
        assert_eq!(lambda_param("o => o.x, k => k."), Some("k".into()));
        assert_eq!(lambda_param("total, by=x => x."), Some("x".into()));
        assert_eq!(lambda_param("a == b"), None);
        assert_eq!(arg_index("o => f(a, b), \"x,y\", "), 2);
        assert_eq!(
            split_callee("Order.where(o => o.x).sum"),
            ("Order.where(o => o.x)", "sum")
        );
        assert_eq!(split_callee("revenue"), ("", "revenue"));
        assert_eq!(
            pick_form("sum() · sum(x => v)", 0),
            ("sum(x => v)".to_string(), vec!["x => v".to_string()])
        );
        assert_eq!(
            pick_form("allocate(total, by=x => weight)", 1).1,
            vec!["total".to_string(), "by=x => weight".to_string()]
        );
    }
}
