//! Intermediate representation (IR) of a Quadra program.
//!
//! The IR is the program after parsing and name analysis, with `extend`s merged into their
//! entities. It is plain data (serializable) and is consumed by the type checker, the
//! analyses (`trace`, cycles, fingerprints) and the interpreter.

pub mod files;
pub mod formats;
mod lower;
pub mod print;

pub use lower::lower;
use quadra_sema::FileId;
use quadra_syntax::TextRange;
use serde::Serialize;
use std::collections::HashMap;

/// Source location of an IR element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Span {
    #[serde(skip)]
    pub file: FileId,
    #[serde(skip)]
    pub range: TextRange,
}

impl Span {
    pub fn new(file: FileId, range: TextRange) -> Span {
        Span { file, range }
    }
}

// ---- types --------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Ty {
    /// `Text`, `Money<order.currency>`, `Price<EUR, pcs>`, `Customer`, `order.currency` (in a
    /// generic argument it is a unit or currency path).
    Path {
        segments: Vec<String>,
        args: Vec<Ty>,
        span: Span,
    },
    Optional(Box<Ty>),
    List(Box<Ty>),
    Set(Box<Ty>),
    Tuple(Vec<(String, Ty)>),
    Enum(Vec<Variant>),
    /// Anonymous union: rejected by the analysis, kept for error recovery.
    Union(Vec<Ty>),
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

// ---- expressions --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Lit {
    Number(String),
    Percent(String),
    Str(String),
    Date(String),
    DateTime(String),
    YearMonth(String),
    Bool(bool),
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Implies,
    Coalesce,
    In,
    NotIn,
    Is,
    IsNot,
    Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
    pub spread: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum FPart {
    Text(String),
    /// `{expr}` or `{expr:spec}`
    Expr(Expr, Option<FormatSpec>),
}

/// The format of an f-string interpolation, after the `:`: `[<|>|^][0][width][.precision]`.
/// `{n:05}` zero-pads to 5 characters, `{x:.2}` shows 2 decimals, `{name:<10}` left-aligns
/// in 10 characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FormatSpec {
    pub align: Option<char>,
    pub zero: bool,
    pub width: usize,
    pub precision: Option<u32>,
}

impl FormatSpec {
    pub fn parse(spec: &str) -> Option<FormatSpec> {
        let mut rest = spec;
        let align = match rest.chars().next() {
            Some(c @ ('<' | '>' | '^')) => {
                rest = &rest[1..];
                Some(c)
            }
            _ => None,
        };
        let zero = rest.starts_with('0');
        if zero {
            rest = &rest[1..];
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let width = if digits > 0 {
            rest[..digits].parse().ok()?
        } else {
            0
        };
        rest = &rest[digits..];
        let precision = match rest.strip_prefix('.') {
            Some(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                Some(p.parse().ok()?)
            }
            Some(_) => return None,
            None if rest.is_empty() => None,
            None => return None,
        };
        if spec.is_empty() || (zero && align.is_some()) {
            return None;
        }
        Some(FormatSpec {
            align,
            zero,
            width,
            precision,
        })
    }

    /// Pad a formatted value to the width: numbers right-aligned (zero-padded after the sign
    /// with `0`), everything else left-aligned, unless the spec gives the alignment.
    pub fn pad(&self, text: &str, numeric: bool) -> String {
        let len = text.chars().count();
        if len >= self.width {
            return text.to_string();
        }
        let fill = self.width - len;
        if self.zero && numeric {
            let (sign, digits) = match text.strip_prefix('-') {
                Some(d) => ("-", d),
                None => ("", text),
            };
            return format!("{sign}{}{digits}", "0".repeat(fill));
        }
        match self.align.unwrap_or(if numeric { '>' } else { '<' }) {
            '<' => format!("{text}{}", " ".repeat(fill)),
            '^' => format!(
                "{}{text}{}",
                " ".repeat(fill / 2),
                " ".repeat(fill - fill / 2)
            ),
            _ => format!("{}{text}", " ".repeat(fill)),
        }
    }
}

impl std::fmt::Display for FormatSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(a) = self.align {
            write!(f, "{a}")?;
        }
        if self.zero {
            write!(f, "0")?;
        }
        if self.width > 0 {
            write!(f, "{}", self.width)?;
        }
        if let Some(p) = self.precision {
            write!(f, ".{p}")?;
        }
        Ok(())
    }
}

/// The text of a string literal with its escapes processed: `\n`, `\t`, `\r`, `\"`, `\\`
/// (and `\{`, `\}`); any other backslash is kept as written.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(c @ ('"' | '\\' | '{' | '}')) => out.push(c),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Pattern {
    Wildcard,
    Lit(Lit),
    /// A variant name, an entity type name or a binding (decided by the type checker).
    Name(String),
    Ctor {
        name: String,
        args: Vec<Pattern>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Arm {
    pub patterns: Vec<Pattern>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum ExprKind {
    Lit(Lit),
    FString(Vec<FPart>),
    /// `3 pcs`, `10.50 EUR`, `9.90 EUR/pcs`, `30 days`
    Quantity {
        value: String,
        unit: String,
        per: Option<String>,
    },
    Name(String),
    SelfRef,
    Field {
        base: Box<Expr>,
        name: String,
        optional: bool,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    /// (the body is shared: a closure made of it at every evaluation keeps it without a copy)
    Lambda {
        params: Vec<String>,
        body: std::sync::Arc<Expr>,
    },
    Tuple(Vec<(Option<String>, Expr)>),
    List(Vec<Arg>),
    Set(Vec<Expr>),
    /// `when:` arms; `None` condition = `else`
    When(Vec<(Option<Expr>, Expr)>),
    Match {
        subject: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// Statements; the value is the last expression statement.
    Block(Vec<Stmt>),
    As {
        user: String,
        body: Box<Expr>,
    },
    Fail(Box<Expr>),
    Error,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }

    /// The expressions directly inside this one, in order. A `Block`'s statements are not
    /// visited: callers that must see everything treat a block on its own.
    pub fn for_each_child<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        match &self.kind {
            ExprKind::Lit(_)
            | ExprKind::Quantity { .. }
            | ExprKind::Name(_)
            | ExprKind::SelfRef
            | ExprKind::Error
            | ExprKind::Block(_) => {}
            ExprKind::FString(parts) => {
                for p in parts {
                    if let FPart::Expr(e, _) = p {
                        f(e);
                    }
                }
            }
            ExprKind::Field { base, .. } => f(base),
            ExprKind::Call { callee, args } => {
                f(callee);
                for a in args {
                    f(&a.value);
                }
            }
            ExprKind::Index { base, index } => {
                f(base);
                f(index);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::As { body: expr, .. }
            | ExprKind::Fail(expr) => f(expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                f(cond);
                f(then);
                f(otherwise);
            }
            ExprKind::Lambda { body, .. } => f(body),
            ExprKind::Tuple(items) => {
                for (_, e) in items {
                    f(e);
                }
            }
            ExprKind::List(args) => {
                for a in args {
                    f(&a.value);
                }
            }
            ExprKind::Set(items) => {
                for e in items {
                    f(e);
                }
            }
            ExprKind::When(arms) => {
                for (c, v) in arms {
                    if let Some(c) = c {
                        f(c);
                    }
                    f(v);
                }
            }
            ExprKind::Match { subject, arms } => {
                f(subject);
                for a in arms {
                    f(&a.body);
                }
            }
        }
    }

    /// A copy with each direct child replaced by `f(child)` (a `Block` is copied as is).
    pub fn map_children(&self, f: &mut dyn FnMut(&Expr) -> Expr) -> Expr {
        let b = |e: &Expr, f: &mut dyn FnMut(&Expr) -> Expr| Box::new(f(e));
        let kind = match &self.kind {
            ExprKind::Lit(_)
            | ExprKind::Quantity { .. }
            | ExprKind::Name(_)
            | ExprKind::SelfRef
            | ExprKind::Error
            | ExprKind::Block(_) => self.kind.clone(),
            ExprKind::FString(parts) => ExprKind::FString(
                parts
                    .iter()
                    .map(|p| match p {
                        FPart::Expr(e, spec) => FPart::Expr(f(e), spec.clone()),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            ExprKind::Field {
                base,
                name,
                optional,
            } => ExprKind::Field {
                base: b(base, f),
                name: name.clone(),
                optional: *optional,
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: b(callee, f),
                args: args
                    .iter()
                    .map(|a| Arg {
                        name: a.name.clone(),
                        value: f(&a.value),
                        spread: a.spread,
                    })
                    .collect(),
            },
            ExprKind::Index { base, index } => ExprKind::Index {
                base: b(base, f),
                index: b(index, f),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: op.clone(),
                expr: b(expr, f),
            },
            ExprKind::As { user, body } => ExprKind::As {
                user: user.clone(),
                body: b(body, f),
            },
            ExprKind::Fail(e) => ExprKind::Fail(b(e, f)),
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: op.clone(),
                lhs: b(lhs, f),
                rhs: b(rhs, f),
            },
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => ExprKind::If {
                cond: b(cond, f),
                then: b(then, f),
                otherwise: b(otherwise, f),
            },
            ExprKind::Lambda { params, body } => ExprKind::Lambda {
                params: params.clone(),
                body: std::sync::Arc::new(f(body)),
            },
            ExprKind::Tuple(items) => {
                ExprKind::Tuple(items.iter().map(|(n, e)| (n.clone(), f(e))).collect())
            }
            ExprKind::List(args) => ExprKind::List(
                args.iter()
                    .map(|a| Arg {
                        name: a.name.clone(),
                        value: f(&a.value),
                        spread: a.spread,
                    })
                    .collect(),
            ),
            ExprKind::Set(items) => ExprKind::Set(items.iter().map(|e| f(e)).collect()),
            ExprKind::When(arms) => ExprKind::When(
                arms.iter()
                    .map(|(c, v)| (c.as_ref().map(|c| f(c)), f(v)))
                    .collect(),
            ),
            ExprKind::Match { subject, arms } => ExprKind::Match {
                subject: b(subject, f),
                arms: arms
                    .iter()
                    .map(|a| Arm {
                        patterns: a.patterns.clone(),
                        body: f(&a.body),
                    })
                    .collect(),
            },
        };
        Expr {
            kind,
            span: self.span,
        }
    }
}

// ---- statements ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum StmtKind {
    Let {
        name: String,
        ty: Option<Ty>,
        value: Expr,
    },
    If {
        binding: Option<String>,
        cond: Expr,
        then: Vec<Stmt>,
        otherwise: Option<Vec<Stmt>>,
    },
    For {
        names: Vec<String>,
        iter: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    Require {
        cond: Expr,
        message: Option<Expr>,
    },
    /// `erase c, "the customer asked"`: the `personal` fields of a record and its lines emptied,
    /// their past values taken out of the history (crate docs: Retention)
    Erase {
        target: Expr,
        reason: Option<Expr>,
    },
    Fail(Expr),
    Emit(Expr),
    Expect(Expr),
    ExpectError {
        message: String,
        body: Vec<Stmt>,
    },
    /// Source of a block that must not compile, re-checked by the test runner.
    ExpectCompileError {
        message: String,
        source: String,
    },
    Use(String),
    Clock(Expr),
    /// in a test: the code of an `inbound` run on a message, as when it comes in (`file`: the
    /// text is a path, from the project's folder)
    Receive {
        inbound: String,
        body: Expr,
        file: bool,
    },
    /// in a test: the events of `event` waiting in the outbox delivered (with the `reply` the
    /// other side gave) or failed (with the error): their `on … delivered` / `failed` run
    Deliver {
        event: String,
        reply: Option<Expr>,
        failed: Option<Expr>,
    },
    /// in a test: an event like this one (`OrderPlaced(order=o)`: those fields, the others
    /// whatever they are) was emitted, or (`negated`) was not
    ExpectSent {
        event: Expr,
        negated: bool,
    },
    As {
        user: String,
        body: Vec<Stmt>,
    },
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    Expr(Expr),
    Meta {
        key: String,
        value: Expr,
    },
    /// `verify:` / `expect:` blocks of migrations and changes
    Block {
        keyword: String,
        body: Vec<Stmt>,
    },
    Fn(FnDef),
    Migration {
        keyword: String,
        paths: Vec<Vec<String>>,
        body: Vec<Stmt>,
        arms: Vec<Arm>,
    },
}

// ---- declarations -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum EntityKind {
    Entity,
    Record,
    Trait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum FieldKind {
    Stored,
    Formula,
    LedgerKey,
    LedgerMeasure,
    LedgerAttr,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Where {
    /// `where > 0`
    Compare(BinOp, Expr),
    /// `where 0..4`, `where c => c.len() == 2`, or any boolean expression
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Field {
    pub name: String,
    pub ty: Option<Ty>,
    pub kind: FieldKind,
    pub default: Option<Expr>,
    pub formula: Option<Expr>,
    pub where_: Option<Where>,
    pub required_when: Option<Expr>,
    pub unique: bool,
    pub parent: bool,
    pub via: Option<String>,
    pub through: Option<String>,
    pub per_company: bool,
    /// `at "…"`: where the field is in the document or the row of a record `from` a format
    /// (a path of keys in JSON, a path of elements in XML, a column in CSV, columns in a
    /// fixed-width line)
    pub at: Option<String>,
    /// `format "%d/%m/%Y"`: how a date, an instant or a boolean (`format "S/N"`) is written in
    /// the format of the record
    pub pattern: Option<String>,
    /// `decimals 2`: the decimals a `Decimal` is written with (in a fixed-width line, the
    /// last digits of the number, without a separator)
    pub decimals: Option<u32>,
    /// A `File` field (`File<pdf, image>`, `[File]`): the files it takes and `max 10 MB`
    /// (also set by a `max` on a field of another type, which the checker refuses)
    pub file: Option<files::FileSpec>,
    /// Declared in an `extend` block of another module.
    pub extension: bool,
    /// Redefines only the default of an inherited (trait) field.
    pub default_override: bool,
    /// `retain …`, `expire …`: how long its values are kept
    pub retention: Retention,
    /// `personal`: data about a person, emptied by `erase`
    pub personal: bool,
    pub span: Span,
}

/// How long a value or a record is kept: at least (`retain`: it cannot be erased before) and
/// at most (`expire`: it is erased then).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Retention {
    pub retain: Option<Term>,
    pub expire: Option<Term>,
}

impl Retention {
    pub fn is_empty(&self) -> bool {
        self.retain.is_none() && self.expire.is_none()
    }
}

/// `10 years after date`: a number of units of time after the value of a field of the record
/// (`created_at`, `updated_at` or a stored `Date`/`DateTime` field).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Term {
    pub n: i64,
    /// `hours`, `days`, `weeks`, `months` or `years` (the singular is accepted); anything else
    /// is reported by the checker
    pub unit: String,
    pub after: String,
    pub span: Span,
}

impl Term {
    /// The unit in the plural, or None for a unit of time that is not one.
    pub fn unit(&self) -> Option<&'static str> {
        Some(match self.unit.as_str() {
            "hour" | "hours" => "hours",
            "day" | "days" => "days",
            "week" | "weeks" => "weeks",
            "month" | "months" => "months",
            "year" | "years" => "years",
            _ => return None,
        })
    }

    /// The instant `n` units after `from`; None when it does not exist (past the calendar).
    pub fn after(&self, from: chrono::NaiveDateTime) -> Option<chrono::NaiveDateTime> {
        use chrono::Months;
        let n = self.n.max(0);
        match self.unit()? {
            "hours" => from.checked_add_signed(chrono::Duration::hours(n)),
            "days" => from.checked_add_signed(chrono::Duration::days(n)),
            "weeks" => from.checked_add_signed(chrono::Duration::weeks(n)),
            "months" => from.checked_add_months(Months::new(n as u32)),
            _ => from.checked_add_months(Months::new((n * 12) as u32)),
        }
    }

    /// The instant `n` units before `to` (`history 1 year`: the values replaced before it go).
    pub fn before(&self, to: chrono::NaiveDateTime) -> Option<chrono::NaiveDateTime> {
        use chrono::Months;
        let n = self.n.max(0);
        match self.unit()? {
            "hours" => to.checked_sub_signed(chrono::Duration::hours(n)),
            "days" => to.checked_sub_signed(chrono::Duration::days(n)),
            "weeks" => to.checked_sub_signed(chrono::Duration::weeks(n)),
            "months" => to.checked_sub_months(Months::new(n as u32)),
            _ => to.checked_sub_months(Months::new((n * 12) as u32)),
        }
    }

    /// As written: `10 years after date`.
    pub fn text(&self) -> String {
        format!("{} {} after {}", self.n, self.unit, self.after)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Param {
    pub name: String,
    pub ty: Option<Ty>,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Ty>,
    pub body: Option<Vec<Stmt>>,
    pub builtin: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub per: Vec<String>,
    pub cond: Expr,
    pub message: Expr,
    pub warn: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unique {
    pub fields: Vec<String>,
    pub when: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Frozen {
    pub when: Expr,
    pub except: Vec<String>,
    pub live: Vec<String>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Transition {
    pub from: Vec<String>,
    pub to: String,
    pub by: Option<String>,
    pub when: Option<Expr>,
    pub unless: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Flow {
    pub field: String,
    pub transitions: Vec<Transition>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Action {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Ty>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Post {
    pub ledger: String,
    pub when: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// An option of a record's format: `sep=";"` in `from csv(sep=";")`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FormatOption {
    pub name: String,
    pub value: OptionValue,
    pub span: Span,
}

/// The value of a format option: a literal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum OptionValue {
    Text(String),
    Bool(bool),
    Number(String),
    /// anything else (an expression): refused by the checker
    Other,
}

/// `ns cbc "urn:…"` in a record `from xml` (`prefix` none: the default namespace).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Namespace {
    pub prefix: Option<String>,
    pub uri: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entity {
    pub name: String,
    pub kind: EntityKind,
    pub is_ledger: bool,
    pub temporary: bool,
    /// `load from "file.csv"` (temporary entities used by migrations)
    pub load_from: Option<String>,
    /// `record X from json:`: the external format it is read from (`X.parse(text)`,
    /// `X.parse_rows(text)`) and written to (`x.json()`, `x.xml()`, `rows.csv()`,
    /// `rows.fixed()`); see `formats`
    pub format: Option<String>,
    /// `from csv(sep=";", header=false)`: the options of the format, as written
    pub format_options: Vec<FormatOption>,
    /// `ns cbc "urn:…"` lines of a record `from xml`: the prefixes its paths use (no prefix:
    /// the default namespace)
    pub namespaces: Vec<Namespace>,
    pub traits: Vec<String>,
    pub fields: Vec<Field>,
    pub actions: Vec<Action>,
    pub methods: Vec<FnDef>,
    pub checks: Vec<Check>,
    pub uniques: Vec<Unique>,
    pub frozen: Option<Frozen>,
    /// `live a, b` declared as members (in the entity or in an `extend`): merged into
    /// `frozen.live`; kept with their place for the checker
    pub live_decls: Vec<(String, Span)>,
    pub flows: Vec<Flow>,
    pub posts: Vec<Post>,
    /// `retain …`, `expire …` as members: how long its records are kept
    pub retention: Retention,
    /// `history 1 year`: the values replaced longer ago are taken out of the history (their
    /// digests kept); None: kept for ever
    pub history: Option<Term>,
    /// `history full | last 10 | none`, or the project's `[data] history`: how many versions of
    /// each value the history keeps
    pub keep: Keep,
    /// where `keep` was said: the entity's line, or the project's default (`[data] history`)
    pub keep_span: Option<Span>,
    /// `deletable`, `deletable, trash 30 days`: its records can be deleted
    pub deletable: Option<Deletable>,
    pub span: Span,
}

/// How many versions of each value an entity's history keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum Keep {
    /// every version, for ever (or for the time of `history 1 year`)
    #[default]
    Full,
    /// the last N versions of each value (the one in force among them): the older leave the
    /// history once a day, their digests kept
    Last(u32),
    /// only the values in force: the history keeps nothing of the entity's records, and the
    /// chain of commits does not cover them (the record is read from its table)
    None,
}

/// `deletable`: the records of the entity can be deleted (`delete rec`, `DELETE
/// /v1/records/{E}/{id}`, the permission `delete E`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Deletable {
    /// `trash 30 days`: a deleted record goes to the trash first (out of every list, `restore`
    /// brings it back) and is deleted for good when the time is over
    pub trash: Option<Term>,
    pub span: Span,
}

impl Entity {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<Variant>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Const {
    pub name: String,
    pub ty: Option<Ty>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    pub span: Span,
}

/// `outbound Event via connection [path "/…"]`: the events of that kind are delivered by the
/// connection of quadra.toml after their transaction commits (HTTP: a signed JSON webhook to
/// the connection's url and `path`; SMTP: an email made of the event's `to`, `subject`, `body`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Outbound {
    pub event: String,
    pub connection: String,
    pub path: Option<String>,
    /// `as e:` and its block: the code that builds the request from the event (its last
    /// expression: `post(…)`, `put(…)`, `email(…)`…), reading the data as it was when the
    /// event was emitted
    pub binding: Option<String>,
    pub body: Option<Expr>,
    /// `reply ErpAck`: the answer read as a record `from json` (`d.reply` in `on … delivered`)
    pub reply: Option<String>,
    /// the lines of policy at the top of the block
    pub policy: Policy,
    /// `batch 50 within 30 seconds`: up to so many events in one request (the block's binding
    /// is then the list of them), sent when full or when the oldest has waited that long
    pub batch: Option<(u32, Option<Expr>)>,
    /// made by `publish Product to shop as p:`: the name of the record (the event's field
    /// `record`), bound in the policy lines and the code besides the event
    pub publishes: Option<String>,
    pub span: Span,
}

/// `inbound ShopOrder via shop path "/orders" as m: ShopOrderDoc:`: the messages a connection
/// sends to `/in/<connection>/<path>`, read as records of `ty` (a record `from json`), kept raw
/// in the inbox, then handled one by one in a transaction of their own.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Inbound {
    pub name: String,
    pub connection: String,
    pub path: Option<String>,
    pub binding: String,
    pub ty: String,
    /// `key m.id`: a message with a key already received is not handled again (else its hash)
    pub key: Option<Expr>,
    /// `pull "/orders?since={cursor}" every 5 minutes`: the messages are fetched, not sent
    pub pull: Option<Pull>,
    /// `retry for 1 hour`: a message whose code fails is tried again until then (since it came)
    pub retry_for: Option<Expr>,
    /// `backoff 30 seconds .. 10 minutes`: the waits between those tries
    pub backoff: Option<Expr>,
    /// `ordered by m.order_id`: one at a time per value, in the order received; one not done
    /// (waiting to be tried again, rejected) holds back the next of its value
    pub ordered: Option<Expr>,
    /// `latest by m.product using m.updated_at`: a message older (by `using`) than one of the
    /// same value already handled is `superseded`, not handled
    pub latest: Option<(Expr, Expr)>,
    /// `atomic per file` (when not said) or `atomic per row`: how the rows of a file (a record
    /// `from csv` or `from fixed`) are committed
    pub atomic: Option<Atomic>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// How the rows of a file an `inbound` reads are committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Atomic {
    /// all of them in one transaction, or none: a row that fails rejects the file
    File,
    /// each row that works, by chunks; those that fail are reported
    Row,
}

/// How an `inbound` fetches its messages from an API that does not send them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pull {
    /// the path, with `{cursor}` where the cursor goes
    pub path: String,
    /// how often (a fixed duration)
    pub every: Expr,
    /// `items Page.field`: the answer is a `Page`, its `field` the messages; else a list of them
    pub items: Option<(String, String)>,
    /// `cursor m.updated_at`: of each message; the largest is where the next pull starts
    pub cursor: Option<Expr>,
}

/// `endpoint GET "/price" via pos as q: PriceQuery -> PosPrice:`: a request of a connection to
/// `/in/<connection><path>` answered at once by the code (its last expression, a record `from
/// json`). A `GET` only reads; a `POST` writes in a transaction, kept in the inbox like a
/// message, and answers the same to the same `key`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    pub connection: String,
    pub binding: String,
    pub query: String,
    pub answer: String,
    pub key: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

/// How an outbound delivers (`outbound … as e:`, its first lines). The durations are fixed
/// (`3 days`); `unless`, `key`, `ordered by` and `latest by` are read for each event.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Policy {
    /// `retry for 3 days`: how long it is tried (a day when not said)
    pub retry_for: Option<Expr>,
    /// `retry 10 times`: how many attempts at most
    pub retry_times: Option<Expr>,
    /// `backoff 30 seconds .. 1 hour`: the first wait, doubled up to the second
    pub backoff: Option<Expr>,
    /// `expires after 1 day`: after that, not sent (`expired`)
    pub expires: Option<Expr>,
    /// `unless e.order.cancelled`: read on the data as it is now before each attempt; true: not
    /// sent (`skipped`)
    pub unless: Option<Expr>,
    /// `delay 10 minutes`: not before that long after the emit
    pub delay: Option<Expr>,
    /// `window "02:00" .. "04:00"`: only between those times of day, in the project's zone
    pub window: Option<Expr>,
    /// `key f"ship-{e.order.number}"`: the idempotency key (else the event's own)
    pub key: Option<Expr>,
    /// `ordered by e.customer`: one at a time per value, in the order emitted
    pub ordered: Option<Expr>,
    /// `latest by e.product`: an event waiting is superseded by a newer one of the same value
    pub latest: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Trigger {
    Enters(String),
    Becomes(Expr),
    Created,
    Changes,
    Before(String),
    /// `on Event delivered as d:` (the reaction's `entity` is the event): its delivery succeeded
    Delivered,
    /// `on Event failed as f:`: its delivery failed for good
    Failed,
    /// `on Event expired as x:`: not sent, `expires after` passed
    Expired,
    /// `on Event skipped as x:`: not sent, its `unless` held
    Skipped,
    /// `on OrderIn rejected as r:` (the reaction's `entity` is the inbound): a message it
    /// received was rejected for good
    Rejected,
}

impl Trigger {
    /// About the outcome of a delivery, or of a message received: not about records.
    pub fn is_outcome(&self) -> bool {
        matches!(
            self,
            Trigger::Delivered
                | Trigger::Failed
                | Trigger::Expired
                | Trigger::Skipped
                | Trigger::Rejected
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reaction {
    pub entity: String,
    pub trigger: Trigger,
    pub binding: String,
    pub converges: bool,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rule {
    /// `allow` (a rule in a role, or `allow …` at the top level), `deny`, `hide`, `show`
    pub effect: String,
    pub verbs: Vec<String>,
    pub history: bool,
    /// `*` or `Entity` or `Entity.member…`
    pub targets: Vec<Vec<String>>,
    pub condition: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Role {
    pub name: String,
    pub parent: Option<String>,
    pub rules: Vec<Rule>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Block {
    pub name: String,
    pub extends: Option<String>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metric {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Ty>,
    pub meta: Vec<(String, Expr)>,
    pub formulas: Vec<(String, Expr)>,
    pub control: bool,
    pub span: Span,
}

/// The zero of an amount, a quantity or a price (`0 EUR`, `0 pcs`, `0 EUR/pcs`), for the
/// places where a program computes a bare `0`: an empty `sum`, `default 0`, `x = 0`.
#[derive(Clone, Debug, PartialEq)]
pub struct Zero {
    pub kind: ZeroKind,
    /// the currency (and, for a price, the unit): a code, or a path from `self`, a local
    /// name, `current` or `company`
    pub units: Vec<UnitRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroKind {
    Money,
    Qty,
    Price,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UnitRef {
    Code(String),
    Path(Vec<String>),
}

/// `document InvoicePdf(inv: Invoice):` or `label ParcelLabel(p: Parcel) width 100 …:` of a
/// `.qdp` file: a function of its parameters to a page layout, rendered by `render(…)` (a PDF)
/// or, for a label, also `zpl(…)`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Document {
    pub name: String,
    pub label: bool,
    pub params: Vec<Param>,
    /// the options of its first line (a label's `width`, `height`, `dpi`)
    pub opts: Vec<PrintOpt>,
    pub items: Vec<PrintItem>,
    pub span: Span,
}

/// An option of an element: `size 14`, `bold` (no value), `align right`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrintOpt {
    pub name: String,
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrintItem {
    pub kind: PrintKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum PrintKind {
    /// `text inv.number size 14 bold`, `table inv.lines as l:` and its children,
    /// `column "Qty" align right: l.qty` (`body` is what follows the `:`)
    Element {
        element: String,
        value: Option<Expr>,
        opts: Vec<PrintOpt>,
        binding: Option<String>,
        body: Option<Expr>,
        children: Vec<PrintItem>,
    },
    Let {
        name: String,
        value: Expr,
    },
    /// `if cond:`, or `if let name = value:` (the branch when the value is not none)
    If {
        bind: Option<String>,
        cond: Expr,
        then: Vec<PrintItem>,
        otherwise: Vec<PrintItem>,
    },
    For {
        name: String,
        iter: Expr,
        body: Vec<PrintItem>,
    },
}

/// `attach InvoicePdf to Invoice.pdf when state is Posted`: after a commit, each record of
/// the entity for which `when` holds and whose field is empty gets the document rendered
/// from it (with the data of that commit), kept with its hash in the field.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Attach {
    pub document: String,
    pub entity: String,
    pub field: String,
    /// `signed "company"`: signed (PAdES) by that signer of quadra.toml before it is kept
    pub signer: Option<String>,
    pub when: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Model {
    /// the project's time zone (IANA name; empty: UTC)
    pub timezone: String,
    pub entities: Vec<Entity>,
    pub enums: Vec<EnumDef>,
    pub fns: Vec<FnDef>,
    pub consts: Vec<Const>,
    pub events: Vec<Event>,
    pub outbounds: Vec<Outbound>,
    pub inbounds: Vec<Inbound>,
    pub endpoints: Vec<Endpoint>,
    pub documents: Vec<Document>,
    pub attaches: Vec<Attach>,
    /// the files of the project the documents show (`image "assets/logo.png"`), read when the
    /// project is loaded, by their path
    #[serde(skip)]
    pub assets: std::collections::BTreeMap<String, Vec<u8>>,
    pub reactions: Vec<Reaction>,
    pub roles: Vec<Role>,
    /// Top-level `deny` / `allow` rules.
    pub rules: Vec<Rule>,
    pub seeds: Vec<Block>,
    pub fixtures: Vec<Block>,
    pub tests: Vec<Block>,
    pub metrics: Vec<Metric>,
    pub migrations: Vec<Block>,
    /// `change` blocks: data scripts with reason, requester and approval
    pub changes: Vec<Block>,
    /// the variables and connections of quadra.toml
    #[serde(skip)]
    pub manifest: quadra_sema::manifest::Manifest,
    /// the values of the variables where the model runs (the environment's, not the code's):
    /// set by the engine that runs it; `env.NAME` reads them, the default when absent
    #[serde(skip)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(skip)]
    index: rustc_hash::FxHashMap<String, usize>,
    /// variant name → where `variant` finds it first: (0: an enum, 1: an inline enum of a
    /// field), enum or entity, field, variant
    #[serde(skip)]
    variant_index: rustc_hash::FxHashMap<String, (u8, usize, usize, usize)>,
    /// Typed zeros, filled by the type checker: the span of a field (its stored value,
    /// default or formula) or of a `sum(…)` call → the zero it stands for.
    #[serde(skip)]
    pub zeros: HashMap<Span, Zero>,
    /// The record of the rows written by `rows.csv()` and `rows.fixed()`, filled by the type
    /// checker by the span of the method's name: the header of an empty list.
    #[serde(skip)]
    pub row_records: HashMap<Span, String>,
    /// Errors found while lowering (an `extend` that cannot be merged), reported by the
    /// type checker with the others.
    #[serde(skip)]
    pub errors: Vec<(Span, String)>,
    /// The source files, by file id: (path relative to the project, text). Messages about the
    /// data quote the line of the rule they are about.
    #[serde(skip)]
    pub sources: Vec<(String, String)>,
}

impl Model {
    /// Where a span is: the file, the line (from 1) and the text of that line.
    pub fn place(&self, span: Span) -> Option<(String, usize, String)> {
        let (path, text) = self.sources.get(span.file.0 as usize)?;
        let start = usize::from(span.range.start()).min(text.len());
        let line = text[..start].matches('\n').count() + 1;
        let line_text = text.lines().nth(line - 1).unwrap_or("").trim().to_string();
        Some((path.clone(), line, line_text))
    }

    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.index.get(name).map(|&i| &self.entities[i])
    }

    pub fn enum_def(&self, name: &str) -> Option<&EnumDef> {
        self.enums.iter().find(|e| e.name == name)
    }

    pub fn function(&self, name: &str) -> Option<&FnDef> {
        self.fns.iter().find(|f| f.name == name)
    }

    pub fn constant(&self, name: &str) -> Option<&Const> {
        self.consts.iter().find(|c| c.name == name)
    }

    pub fn event(&self, name: &str) -> Option<&Event> {
        self.events.iter().find(|e| e.name == name)
    }

    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }

    pub(crate) fn reindex(&mut self) {
        self.index = self
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), i))
            .collect();
        let mut vi: rustc_hash::FxHashMap<String, (u8, usize, usize, usize)> = Default::default();
        for (i, e) in self.enums.iter().enumerate() {
            for (k, v) in e.variants.iter().enumerate() {
                vi.entry(v.name.clone()).or_insert((0, i, 0, k));
            }
        }
        for (i, ent) in self.entities.iter().enumerate() {
            for (j, f) in ent.fields.iter().enumerate() {
                for (k, v) in
                    f.ty.as_ref()
                        .and_then(|t| inline_enum(t))
                        .into_iter()
                        .flatten()
                        .enumerate()
                {
                    vi.entry(v.name.clone()).or_insert((1, i, j, k));
                }
            }
        }
        self.variant_index = vi;
    }

    /// All fields of an entity, including those inherited from traits (own fields first).
    /// The entities whose past values a frozen record may read (it reads the world as of when
    /// it froze): the frozen ones, their lines, and every entity they reach through references.
    /// Their history is kept whole (no `history`, nothing archived).
    pub fn read_as_of(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut todo: Vec<String> = self
            .entities
            .iter()
            .filter(|e| e.frozen.is_some())
            .map(|e| e.name.clone())
            .collect();
        while let Some(n) = todo.pop() {
            if !out.insert(n.clone()) {
                continue;
            }
            let Some(e) = self.entity(&n) else { continue };
            // its lines
            for l in &self.entities {
                if l.fields
                    .iter()
                    .any(|f| f.parent && type_name(f.ty.as_ref()).as_deref() == Some(n.as_str()))
                {
                    todo.push(l.name.clone());
                }
            }
            for f in self.all_fields(e) {
                if let Some(t) = type_name(f.ty.as_ref()) {
                    if self.entity(&t).is_some() {
                        todo.push(t);
                    }
                }
            }
        }
        out
    }

    pub fn all_fields<'m>(&'m self, entity: &'m Entity) -> Vec<&'m Field> {
        let mut out: Vec<&Field> = entity
            .fields
            .iter()
            .filter(|f| !f.default_override)
            .collect();
        let mut stack: Vec<&str> = entity.traits.iter().map(String::as_str).collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            if let Some(te) = self.entity(t) {
                for f in &te.fields {
                    if !out.iter().any(|o| o.name == f.name) {
                        out.push(f);
                    }
                }
                stack.extend(te.traits.iter().map(String::as_str));
            }
        }
        out
    }

    /// The traits an entity implements, directly or through other traits (nearest first).
    pub fn all_traits<'m>(&'m self, entity: &'m Entity) -> Vec<&'m Entity> {
        let mut out: Vec<&Entity> = Vec::new();
        let mut queue: std::collections::VecDeque<&str> =
            entity.traits.iter().map(String::as_str).collect();
        while let Some(t) = queue.pop_front() {
            let Some(te) = self.entity(t) else { continue };
            if out.iter().any(|o| o.name == te.name) {
                continue;
            }
            out.push(te);
            queue.extend(te.traits.iter().map(String::as_str));
        }
        out
    }

    /// `check`/`warn` rules of an entity, including those of its traits (own rules first).
    pub fn all_checks<'m>(&'m self, entity: &'m Entity) -> Vec<&'m Check> {
        let mut out: Vec<&Check> = entity.checks.iter().collect();
        for t in self.all_traits(entity) {
            out.extend(t.checks.iter());
        }
        out
    }

    /// `unique (…)` constraints of an entity, including those of its traits: a trait's
    /// constraint holds among the records of each implementing entity.
    pub fn all_uniques<'m>(&'m self, entity: &'m Entity) -> Vec<&'m Unique> {
        let mut out: Vec<&Unique> = entity.uniques.iter().collect();
        for t in self.all_traits(entity) {
            out.extend(t.uniques.iter());
        }
        out
    }

    /// A field of an entity or its traits: the first of `all_fields` with that name, found
    /// without building the list (it is called on every read of a field).
    pub fn find_field<'m>(&'m self, entity: &'m Entity, name: &str) -> Option<&'m Field> {
        if let Some(f) = entity
            .fields
            .iter()
            .find(|f| !f.default_override && f.name == name)
        {
            return Some(f);
        }
        if entity.traits.is_empty() {
            return None;
        }
        let mut stack: Vec<&str> = entity.traits.iter().map(String::as_str).collect();
        let mut seen: Vec<&str> = Vec::new();
        while let Some(t) = stack.pop() {
            if seen.contains(&t) {
                continue;
            }
            seen.push(t);
            if let Some(te) = self.entity(t) {
                if let Some(f) = te.fields.iter().find(|f| f.name == name) {
                    return Some(f);
                }
                stack.extend(te.traits.iter().map(String::as_str));
            }
        }
        None
    }

    /// Why `entity.field` cannot be assigned, when it is the field of a flow: it changes only
    /// through the flow's transitions (the actions named with `by`, the automatic ones).
    /// Migrations and `change` blocks may still set it (data fixes).
    pub fn flow_assignment_error(&self, entity: &Entity, field: &str) -> Option<String> {
        let flow = entity.flows.iter().find(|f| f.field == field)?;
        let mut actions: Vec<&str> = Vec::new();
        for t in &flow.transitions {
            if let Some(b) = t.by.as_deref().filter(|b| !actions.contains(b)) {
                actions.push(b);
            }
        }
        let automatic = flow
            .transitions
            .iter()
            .any(|t| t.by.is_none() && t.when.is_some());
        let how = match (actions.is_empty(), automatic) {
            (false, false) => format!("run one of its actions ({})", actions.join(", ")),
            (false, true) => format!(
                "run one of its actions ({}), or let its automatic transitions move it",
                actions.join(", ")
            ),
            (true, _) => "it moves by its automatic transitions".to_string(),
        };
        Some(format!(
            "`{}.{field}` changes only through its flow: {how}",
            entity.name
        ))
    }

    /// The state a new record starts in, for the field of a flow whose default names a state.
    pub fn initial_state(&self, entity: &Entity, field: &str) -> Option<String> {
        entity.flows.iter().find(|f| f.field == field)?;
        let f = self.find_field(entity, field)?;
        match &f.default.as_ref()?.kind {
            ExprKind::Name(n) => Some(n.clone()),
            _ => None,
        }
    }

    /// The entity whose `frozen when` freezes the records of `entity`: the entity itself, or
    /// (for the lines of a document: records with a `parent` field) the nearest entity up the
    /// chain of `parent` fields that has one. A parent typed by a trait counts when one of its
    /// implementors has `frozen when`.
    pub fn frozen_owner<'m>(&'m self, entity: &'m Entity) -> Option<&'m Entity> {
        let mut cur = entity;
        for _ in 0..10 {
            if cur.frozen.is_some() {
                return Some(cur);
            }
            let pf = self.all_fields(cur).into_iter().find(|f| f.parent)?;
            let name = match pf.ty.as_ref()? {
                Ty::Path { segments, .. } if segments.len() == 1 => segments[0].clone(),
                Ty::Optional(t) => match &**t {
                    Ty::Path { segments, .. } if segments.len() == 1 => segments[0].clone(),
                    _ => return None,
                },
                _ => return None,
            };
            let pe = self.entity(&name)?;
            if pe.kind == EntityKind::Trait {
                return self
                    .implementors(&name)
                    .into_iter()
                    .find_map(|i| self.frozen_owner(i));
            }
            cur = pe;
        }
        None
    }

    /// Entities implementing a trait (directly or through other traits).
    pub fn implementors(&self, trait_name: &str) -> Vec<&Entity> {
        self.entities
            .iter()
            .filter(|e| e.kind != EntityKind::Trait && self.implements(e, trait_name))
            .collect()
    }

    pub fn implements(&self, entity: &Entity, trait_name: &str) -> bool {
        if entity.name == trait_name {
            return true;
        }
        entity.traits.iter().any(|t| {
            t == trait_name
                || self
                    .entity(t)
                    .map_or(false, |te| self.implements(te, trait_name))
        })
    }

    /// Enum (named or inline) that declares a variant, with its definition.
    pub fn variant(&self, name: &str) -> Option<&Variant> {
        // (the index says where the first one of that name is: the order of the search below)
        if let Some(&(kind, i, j, k)) = self.variant_index.get(name) {
            let v = match kind {
                0 => self.enums.get(i).and_then(|e| e.variants.get(k)),
                _ => self
                    .entities
                    .get(i)
                    .and_then(|e| e.fields.get(j))
                    .and_then(|f| f.ty.as_ref())
                    .and_then(|t| inline_enum(t))
                    .and_then(|vs| vs.get(k)),
            };
            if let Some(v) = v.filter(|v| v.name == name) {
                return Some(v);
            }
        }
        for e in &self.enums {
            if let Some(v) = e.variants.iter().find(|v| v.name == name) {
                return Some(v);
            }
        }
        for ent in &self.entities {
            for f in &ent.fields {
                if let Some(v) =
                    f.ty.as_ref()
                        .and_then(|t| inline_enum(t))
                        .and_then(|vs| vs.iter().find(|v| v.name == name))
                {
                    return Some(v);
                }
            }
        }
        None
    }
}

/// The variants of an inline `enum(...)` type (through `?`).
pub fn inline_enum(ty: &Ty) -> Option<&Vec<Variant>> {
    match ty {
        Ty::Enum(vs) => Some(vs),
        Ty::Optional(inner) => inline_enum(inner),
        _ => None,
    }
}

/// The name a type refers to, through `?`, lists and sets: `Customer`, `[Line]`, `Address?`.
fn type_name(t: Option<&Ty>) -> Option<String> {
    match t? {
        Ty::Path { segments, .. } => segments.last().cloned(),
        Ty::Optional(i) | Ty::List(i) | Ty::Set(i) => type_name(Some(i)),
        _ => None,
    }
}
