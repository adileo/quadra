//! The interpreter: expressions, statements, records, actions, flows, reactions, ledgers,
//! constraints and permissions.

use crate::store::{Cause, LedgerRow, Record, Store};
use crate::time;
use crate::value::{list, text, Closure, Id, V};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use quadra_ir::*;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use quadra_analysis::deps;
mod incr;
mod rollup;
pub use rollup::{
    COMPUTED as FORMULAS_COMPUTED, DELTAS as FORMULAS_BY_DELTA, STORED as FORMULAS_STORED,
};
mod documents;
mod grouping;
mod query;
mod scripts;
mod viewer;
pub use query::{Groups, KeyVal, Page, Spec};
pub use scripts::{ChangeDiff, ScriptMeta};

/// Non-local exits.
#[derive(Debug, Clone)]
pub enum Flow {
    /// A user-visible failure: `fail`, `require`, a violated `check`, a permission…
    Fail(String),
    /// `return` from a function or action
    Return(V),
    /// The commit was refused: others committed changes to what this transaction read.
    Conflict(Vec<Conflict>),
}

/// A transaction committed by someone else after this one started, touching what it read.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub tx: u64,
    pub at: String,
    pub user: Option<String>,
    pub cause: String,
    /// what it changed that this transaction had read (`Order#12.state`, `Stock rows for …`)
    pub what: Vec<String>,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transaction {} by {} at {} ({}): {}",
            self.tx,
            self.user.as_deref().unwrap_or("system"),
            self.at,
            self.cause,
            self.what.join(", ")
        )
    }
}

/// Why a commit did not happen.
#[derive(Debug, Clone)]
pub enum CommitError {
    Conflict(Vec<Conflict>),
    Failed(String),
}

pub type R<T> = Result<T, Flow>;

impl<'m> Interp<'m> {
    /// `post(path, body)`, `put`, `patch`, `delete(path)`, `email(to=, subject=, body=, html=)`:
    /// what an `outbound … as e:` block sends, as a tuple the engine reads (`method`, `path`,
    /// `body`, `content_type`; for an email `to`, `subject`, `body`, `html`). A body that is a
    /// record `from json` is written as its document; `print(content)` gives the bytes for a
    /// printer.
    fn request(&mut self, n: &str, pos: Vec<V>, named: Vec<(String, V)>) -> R<V> {
        let get = |k: &str, i: Option<usize>| -> V {
            named
                .iter()
                .find(|(x, _)| x == k)
                .map(|(_, v)| v.clone())
                .or_else(|| i.and_then(|i| pos.get(i).cloned()))
                .unwrap_or(V::None)
        };
        let as_text = |v: &V| -> Option<String> {
            match v {
                V::None => None,
                V::Text(t) => Some(t.to_string()),
                other => Some(other.to_string()),
            }
        };
        let mut out: Vec<(Option<String>, V)> = Vec::new();
        if n == "insert" {
            // a row: the record's fields as plain JSON, by their `at`
            let row = match get("row", Some(1)) {
                rec @ V::Rec { .. } => crate::formats::render_json(self.m, &rec).or_else(fail)?,
                other => return fail(format!("insert takes a record, not {other}")),
            };
            out.push((Some("method".into()), text("INSERT")));
            out.push((Some("path".into()), get("table", Some(0))));
            out.push((Some("body".into()), text(&row)));
            out.push((Some("content_type".into()), text("application/json")));
            return Ok(V::Tuple(Rc::new(out)));
        }
        if n == "write" {
            let (body, ctype) = match get("content", Some(1)) {
                // a document rendered (`render(…)`): its bytes, a character per byte
                V::Tuple(f) if f.iter().any(|(k, _)| k.as_deref() == Some("content")) => {
                    use base64::Engine as _;
                    let field = |k: &str| {
                        f.iter()
                            .find(|(n, _)| n.as_deref() == Some(k))
                            .map(|(_, v)| match v {
                                V::Text(t) => t.to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default()
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(field("content"))
                        .or_else(|_| fail("the document's bytes are not readable"))?;
                    out.push((Some("encoding".into()), text("bytes")));
                    (bytes.iter().map(|b| *b as char).collect(), field("type"))
                }
                rec @ V::Rec { .. } => {
                    let (body, ctype) = crate::formats::render(self.m, &rec).or_else(fail)?;
                    (body, ctype.to_string())
                }
                // the rows of a file: its bytes in the record's encoding (latin1, EBCDIC),
                // carried a character per byte
                V::List(rows) => {
                    let bytes = match rows.first() {
                        Some(V::Rec { ent, .. }) => {
                            crate::formats::render_rows_bytes(self.m, ent, &rows).or_else(fail)?
                        }
                        _ => Vec::new(),
                    };
                    out.push((Some("encoding".into()), text("bytes")));
                    (
                        bytes.iter().map(|b| *b as char).collect(),
                        "application/octet-stream".into(),
                    )
                }
                other => (as_text(&other).unwrap_or_default(), "text/plain".into()),
            };
            out.push((Some("method".into()), text("WRITE")));
            out.push((Some("path".into()), get("path", Some(0))));
            out.push((Some("body".into()), text(&body)));
            out.push((Some("content_type".into()), text(&ctype)));
            return Ok(V::Tuple(Rc::new(out)));
        }
        if n == "print" {
            // a document rendered: its bytes, a character per byte; text (ZPL) as it is
            let (body, ctype) = match get("content", Some(0)) {
                V::Tuple(f) if f.iter().any(|(k, _)| k.as_deref() == Some("content")) => {
                    use base64::Engine as _;
                    let field = |k: &str| {
                        f.iter()
                            .find(|(n, _)| n.as_deref() == Some(k))
                            .map(|(_, v)| match v {
                                V::Text(t) => t.to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default()
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(field("content"))
                        .or_else(|_| fail("the document's bytes are not readable"))?;
                    out.push((Some("encoding".into()), text("bytes")));
                    (bytes.iter().map(|b| *b as char).collect(), field("type"))
                }
                V::None => return fail("`print` has nothing to print"),
                other => (as_text(&other).unwrap_or_default(), "text/plain".into()),
            };
            out.push((Some("method".into()), text("PRINT")));
            out.push((Some("body".into()), text(&body)));
            out.push((Some("content_type".into()), text(&ctype)));
            return Ok(V::Tuple(Rc::new(out)));
        }
        if n == "email" {
            out.push((Some("method".into()), text("EMAIL")));
            for k in ["to", "subject", "body", "html", "attachments"] {
                out.push((Some(k.into()), get(k, None)));
            }
        } else {
            let path = as_text(&get("path", Some(0))).unwrap_or_default();
            let (body, ctype) = match get("body", Some(1)) {
                V::None => (V::None, V::None),
                rec @ V::Rec { .. } => {
                    let (body, ctype) = crate::formats::render(self.m, &rec).or_else(fail)?;
                    (text(&body), text(ctype))
                }
                other => (
                    as_text(&other).map(|t| text(&t)).unwrap_or(V::None),
                    text("text/plain; charset=utf-8"),
                ),
            };
            out.push((Some("method".into()), text(&n.to_uppercase())));
            out.push((Some("path".into()), text(&path)));
            out.push((Some("body".into()), body));
            out.push((Some("content_type".into()), ctype));
        }
        Ok(V::Tuple(Rc::new(out)))
    }

    /// `env.NAME`: the value of a variable where the model runs (its default when the
    /// environment has none), as its declared type.
    fn env_value(&self, name: &str) -> R<V> {
        use quadra_sema::manifest::VarType;
        let Some(var) = self.m.manifest.var(name) else {
            return fail(format!("quadra.toml declares no variable {name}"));
        };
        let Some(raw) = self.m.manifest.value(&self.m.env, name) else {
            return fail(format!(
                "the variable {name} is not set where the app runs (quadra.toml, [vars])"
            ));
        };
        let raw = raw.trim();
        let bad = || {
            fail(format!(
                "the value of {name} is not {} {}",
                if var.ty == VarType::Integer {
                    "an"
                } else {
                    "a"
                },
                var.ty.name()
            ))
        };
        Ok(match var.ty {
            VarType::Text => text(raw),
            VarType::Integer => match raw.parse() {
                Ok(i) => V::Int(i),
                Err(_) => return bad(),
            },
            VarType::Decimal => match raw.parse() {
                Ok(d) => V::Dec(d),
                Err(_) => return bad(),
            },
            VarType::Boolean => match raw {
                "true" => V::Bool(true),
                "false" => V::Bool(false),
                _ => return bad(),
            },
        })
    }
}

fn fail<T>(msg: impl Into<String>) -> R<T> {
    Err(Flow::Fail(msg.into()))
}

/// The local names of a scope.
pub type Scope = rustc_hash::FxHashMap<String, V>;

#[derive(Clone, Default)]
pub struct Ctx {
    pub this: Option<V>,
    pub entity: Option<String>,
    pub scopes: Vec<Scope>,
    /// company of a per-company member being computed
    pub company: Option<Id>,
    /// read the world as of this transaction (frozen records)
    pub as_of: Option<u64>,
}

impl Ctx {
    pub fn local(&self, name: &str) -> Option<&V> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    pub fn bind(&mut self, name: &str, v: V) {
        if self.scopes.is_empty() {
            self.scopes.push(Default::default());
        }
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }

    /// `x = v` on an existing local: updates the innermost scope that declares it (a `for` or
    /// `if` body assigning a variable of the enclosing block changes that variable).
    pub fn assign(&mut self, name: &str, v: V) {
        match self.scopes.iter_mut().rev().find(|s| s.contains_key(name)) {
            Some(scope) => {
                scope.insert(name.to_string(), v);
            }
            None => self.bind(name, v),
        }
    }

    fn for_record(r: &V, entity: &str) -> Ctx {
        Ctx {
            this: Some(r.clone()),
            entity: Some(entity.to_string()),
            scopes: vec![Default::default()],
            company: None,
            as_of: None,
        }
    }
}

/// What a committed transaction changed: the database writes it in the same transaction.
pub struct Commit<'a> {
    /// the local transaction number (the database may assign another: see `CommitSink`)
    pub tx: u64,
    /// the committed transaction the working set was read at
    pub base: u64,
    pub before: &'a Store,
    pub after: &'a Store,
    pub user: Option<Id>,
    pub cause: String,
    pub time: NaiveDateTime,
}

/// Persistence: called after the engine has accepted a transaction. An error rolls the
/// transaction back in memory as well.
pub trait CommitSink {
    /// Returns the transaction number the database assigned.
    fn commit(&mut self, commit: &Commit) -> Result<u64, CommitError>;
}

/// The analysis of a model that interpreters look up while they run, kept once computed:
/// dependencies of rules and formulas by key, and which formulas are incremental (by name, and
/// by the address of the entity and field in the model). Valid for one model only.
#[derive(Default)]
pub struct Statics {
    pub(crate) deps: std::cell::RefCell<HashMap<String, Rc<deps::Deps>>>,
    pub(crate) trusted: std::cell::RefCell<HashMap<(String, String), bool>>,
    pub(crate) trusted_ptr: std::cell::RefCell<HashMap<(usize, usize), bool>>,
}

/// The most elements a list made at once holds (a range), whatever the job's own limit.
pub const MAX_VALUES: usize = 10_000_000;
/// The longest text a concatenation makes.
pub const MAX_TEXT: usize = 64 << 20;

pub struct Interp<'m> {
    /// Persistence of committed transactions (None = in memory only).
    pub sink: Option<Box<dyn CommitSink + 'm>>,
    /// Formulas whose values are written to the database: (entity, field).
    pub materialize: Vec<(String, String)>,
    pub m: &'m Model,
    pub store: Store,
    pub clock: NaiveDateTime,
    pub user: Option<Id>,
    pub company: Option<Id>,
    pub cause: Rc<Cause>,
    /// > 0 while running actions, reactions, fixtures: permissions are not checked
    pub system: usize,
    /// > 0 while running a migration or a `change`: data fixes may set the field of a flow
    pub script: usize,
    ctor_depth: usize,
    post_buffer: Option<Vec<HashMap<String, V>>>,
    /// `post_written` is running / the store's write count when it last ran
    posting_early: bool,
    posted_at_writes: u64,
    units: HashMap<String, (Decimal, String, Decimal)>,
    currencies: HashMap<String, u32>,
    /// the record of each currency, by code (its `rounding` is read when rounding)
    currency_ids: HashMap<String, Id>,
    local_fns: HashMap<String, (FnDef, Vec<(String, V)>)>,
    depth: usize,
    /// dependencies of rules, for incremental evaluation with storage on demand
    /// what the model's analysis says (dependencies, incremental formulas): the same for every
    /// interpreter of one model, so an engine shares it between its requests (`Statics`)
    pub statics: Rc<Statics>,
    /// re-evaluate the rules that read the clock on every record (a periodic sweep)
    pub sweep: bool,
    /// evaluating the block of an `outbound … as e:`: `post(…)`, `email(…)`… build its request
    pub building_request: bool,
    /// in a test: every event emitted by a committed transaction, in order (`expect sent`,
    /// `deliver`), with whether it is still to be delivered
    pub sent: Option<Vec<(V, bool)>>,
    /// when the previous sweep ran: what a `becomes` condition that reads the clock was then
    pub last_sweep: Option<NaiveDateTime>,
    /// checking every existing record against a new model (a deploy): the broken rules are
    /// collected with every record that breaks them, since nobody is editing those records
    pub whole_data: bool,
    pub violations: Vec<Violation>,
    /// Values of formulas, kept while the data cannot change (a bulk import evaluates the
    /// formulas and rules of millions of records over the same data): a formula read again
    /// is not computed again. `None`: every read computes (the engine's normal behaviour).
    pub formula_memo: Option<HashMap<(Id, &'m str), V>>,
    /// The incremental formulas: kept up to date by each commit, their stored values are read
    /// instead of computing them (`rollup.rs`)
    inc: rollup::Inc<'m>,
    /// While the commit checks the rules and writes the formulas: a rollup computed by delta
    /// then is a commutative change (`Store::deltas`) instead of a read of its stored value.
    pub(crate) delta_free: bool,
    /// The rollups computed by delta while a rule is evaluated (to tell a guard from a read).
    pub(crate) capture: Option<Vec<(Id, String)>>,
    /// The formula being materialized: a rollup of its own computed by delta is a change that
    /// adds up; the same value read by another formula computed in full is a read.
    pub(crate) materializing: Option<(Id, String)>,
    /// > 0 while a rollup's change is being computed: the children's values it reads only
    /// measure that change (the change is the same whatever others added meanwhile).
    pub(crate) in_delta: usize,
    /// Expressions written by a user (cells of an app) evaluated as that user: queries keep
    /// the rows they may read, hidden fields and records they may not read fail, the
    /// interpreter reads at most `viewer::ENGINE_ROWS` rows of an entity (viewer.rs).
    pub viewer: bool,
    /// > 0 while the model's own code runs for the viewer (a formula of a record, a permission
    /// rule): it reads what the code reads
    viewer_pause: usize,
    viewer_seen: HashMap<Id, bool>,
    /// > 0 inside a lambda (`o => o.customer.name`): what rows reach through references is read
    /// as the database's queries read it (the API's filters), with or without SQL alike; only
    /// hidden fields are refused there
    viewer_in_query: usize,
    /// evaluation stops with an error after this instant: the job's deadline (limits.rs), or
    /// a shorter one (the formulas of a workbook)
    pub deadline: Option<std::time::Instant>,
    /// a list or a set may not grow beyond this many elements (limits.rs)
    pub max_list: Option<usize>,
    ticks: u32,
    /// the project's time zone: where an instant becomes a date, an hour, a weekday
    pub tz: chrono_tz::Tz,
}

/// Nested calls of formulas, functions, actions and lambdas allowed in one evaluation.
pub const MAX_DEPTH: usize = 200;

/// The units of measure and currencies of a database, as `load_units` reads them: an engine
/// keeps them between requests while no `Uom` or `Currency` record changes.
#[derive(Clone, Debug, Default)]
pub struct Units {
    units: HashMap<String, (Decimal, String, Decimal)>,
    currencies: HashMap<String, u32>,
    currency_ids: HashMap<String, Id>,
}

/// The rules of an entity a deploy checks on its existing records (`check_record_sel`).
#[derive(Clone, Debug, Default)]
pub struct RuleSel {
    /// every rule
    pub all: bool,
    /// fields whose `required` / `required when` is checked
    pub required: std::collections::BTreeSet<String>,
    /// fields whose `where` is checked
    pub wheres: std::collections::BTreeSet<String>,
    /// fields holding `record` values whose rules are checked
    pub records: std::collections::BTreeSet<String>,
    /// `check`s, by their position in `Model::all_checks`
    pub checks: std::collections::BTreeSet<usize>,
    /// the warnings are computed again (every `warn` of the entity and of its record values)
    pub warnings: bool,
}

impl RuleSel {
    pub fn is_empty(&self) -> bool {
        !self.all
            && self.required.is_empty()
            && self.wheres.is_empty()
            && self.records.is_empty()
            && self.checks.is_empty()
            && !self.warnings
    }

    pub fn merge(&mut self, o: &RuleSel) {
        self.all |= o.all;
        self.required.extend(o.required.iter().cloned());
        self.wheres.extend(o.wheres.iter().cloned());
        self.records.extend(o.records.iter().cloned());
        self.checks.extend(o.checks.iter().copied());
        self.warnings |= o.warnings;
    }
}

/// A rule the existing data breaks, found while checking every record against a new model.
#[derive(Clone, Debug)]
pub struct Violation {
    pub entity: String,
    pub span: quadra_ir::Span,
    pub message: String,
    /// the first records that break it: the record, and the fields the rule reads with their values
    pub records: Vec<(Id, String, Vec<(String, String)>)>,
    pub count: usize,
}

/// How many records of one rule a message lists.
const VIOLATIONS_SHOWN: usize = 5;

fn d(i: i64) -> Decimal {
    Decimal::from(i)
}

fn parse_dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or_default()
}

fn hundred() -> Decimal {
    Decimal::from(100)
}

impl<'m> Interp<'m> {
    pub fn new(m: &'m Model) -> Interp<'m> {
        let clock = NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        Interp {
            sink: None,
            materialize: Vec::new(),
            m,
            store: Store::default(),
            clock,
            user: None,
            company: None,
            cause: Rc::new(Cause {
                desc: "setup".into(),
                user: "system".into(),
                time: clock,
                parent: None,
            }),
            system: 0,
            script: 0,
            ctor_depth: 0,
            post_buffer: None,
            posting_early: false,
            posted_at_writes: 0,
            units: HashMap::new(),
            currencies: HashMap::new(),
            currency_ids: HashMap::new(),
            local_fns: HashMap::new(),
            depth: 0,
            statics: Rc::new(Statics::default()),
            sweep: false,
            building_request: false,
            sent: None,
            last_sweep: None,
            whole_data: false,
            violations: Vec::new(),
            formula_memo: None,
            inc: rollup::Inc::default(),
            delta_free: false,
            capture: None,
            materializing: None,
            in_delta: 0,
            viewer: false,
            viewer_pause: 0,
            viewer_seen: HashMap::new(),
            viewer_in_query: 0,
            deadline: crate::limits::current().deadline,
            max_list: crate::limits::current().max_list,
            ticks: 0,
            tz: time::zone(&m.timezone).unwrap_or(chrono_tz::Tz::UTC),
        }
    }

    // ---- model helpers ----------------------------------------------------------------

    fn entity(&self, name: &str) -> R<&'m Entity> {
        match self.m.entity(name) {
            Some(e) => Ok(e),
            None => fail(format!("unknown entity `{name}`")),
        }
    }

    fn field(&self, ent: &str, name: &str) -> Option<&'m Field> {
        self.m.entity(ent).and_then(|e| self.m.find_field(e, name))
    }

    /// The entity of a record ("" when there is none).
    fn ent_of(&mut self, id: Id) -> Rc<str> {
        self.store
            .get(id)
            .map(|r| r.ent.clone())
            .unwrap_or_else(|| Rc::from(""))
    }

    /// Records of an entity (or of the implementors of a trait).
    pub fn ids(&mut self, ent: &str, as_of: Option<u64>) -> Vec<Id> {
        let mut out: Vec<Id> = Vec::new();
        let names: Vec<String> = match self.m.entity(ent) {
            Some(e) if e.kind == EntityKind::Trait => self
                .m
                .implementors(ent)
                .iter()
                .map(|e| e.name.clone())
                .collect(),
            _ => vec![ent.to_string()],
        };
        for n in names {
            for id in self.store.ids_of(&n) {
                if self.store.exists(id, as_of) {
                    out.push(id);
                }
            }
        }
        out.sort();
        out
    }

    fn user_name(&mut self) -> String {
        if let Some(u) = self.user {
            self.store.load_as(u, "User");
        }
        self.user
            .and_then(|u| self.store.read(u, "login", None))
            .map(|v| v.to_string())
            .unwrap_or_else(|| "system".into())
    }

    pub fn with_cause<T>(&mut self, desc: String, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let parent = self.cause.clone();
        self.cause = Rc::new(Cause {
            desc,
            user: self.user_name(),
            time: self.clock,
            parent: Some(parent.clone()),
        });
        let r = f(self);
        self.cause = parent;
        r
    }

    // ---- units and currencies ---------------------------------------------------------

    /// Refresh the unit and currency tables from the seeded records.
    /// Set the clock before any work (the initial "setup" cause takes the same time).
    pub fn set_clock(&mut self, t: NaiveDateTime) {
        self.clock = t;
        if self.cause.parent.is_none() && self.cause.desc == "setup" {
            self.cause = Rc::new(Cause {
                desc: "setup".into(),
                user: "system".into(),
                time: t,
                parent: None,
            });
        }
    }

    pub fn load_units(&mut self) {
        for id in self.ids("Uom", None) {
            let code = self
                .store
                .read(id, "code", None)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let factor = self
                .store
                .read(id, "factor", None)
                .and_then(|v| v.num())
                .unwrap_or(Decimal::ONE);
            let precision = self
                .store
                .read(id, "precision", None)
                .and_then(|v| v.num())
                .unwrap_or(parse_dec("0.001"));
            let cat = self
                .store
                .read(id, "category", None)
                .and_then(|v| v.as_ref_id())
                .unwrap_or(0);
            self.units
                .insert(code, (factor, cat.to_string(), precision));
        }
        for id in self.ids("Currency", None) {
            let code = self
                .store
                .read(id, "code", None)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let dec = self
                .store
                .read(id, "decimals", None)
                .and_then(|v| v.num())
                .and_then(|n| n.to_u32())
                .unwrap_or(2);
            self.currencies.insert(code.clone(), dec);
            self.currency_ids.insert(code, id);
        }
    }

    /// The units and currencies loaded (`load_units`).
    pub fn units(&self) -> Units {
        Units {
            units: self.units.clone(),
            currencies: self.currencies.clone(),
            currency_ids: self.currency_ids.clone(),
        }
    }

    /// Units and currencies read before (instead of `load_units`).
    pub fn set_units(&mut self, u: &Units) {
        self.units = u.units.clone();
        self.currencies = u.currencies.clone();
        self.currency_ids = u.currency_ids.clone();
    }

    fn decimals(&self, cur: &str) -> u32 {
        self.currencies.get(cur).copied().unwrap_or(2)
    }

    fn convert_qty(&self, v: Decimal, from: &str, to: &str) -> R<Decimal> {
        if from == to {
            return Ok(v);
        }
        match (self.units.get(from), self.units.get(to)) {
            (Some((f1, c1, _)), Some((f2, c2, _))) if c1 == c2 => Ok(tidy(v * f1 / f2)),
            _ => fail(format!("cannot convert {from} to {to}")),
        }
    }

    fn factors(&self, from: &str, to: &str) -> R<(Decimal, Decimal)> {
        if from == to {
            return Ok((Decimal::ONE, Decimal::ONE));
        }
        match (self.units.get(from), self.units.get(to)) {
            (Some((f1, c1, _)), Some((f2, c2, _))) if c1 == c2 => Ok((*f1, *f2)),
            _ => fail(format!("cannot convert {from} to {to}")),
        }
    }

    fn round_money(&mut self, amt: Decimal, cur: &str) -> Decimal {
        amt.round_dp_with_strategy(self.decimals(cur), self.rounding(cur))
    }

    /// The rounding mode of a currency (`Currency.rounding`): half-even unless it says otherwise.
    fn rounding(&mut self, cur: &str) -> RoundingStrategy {
        let mode = self
            .currency_ids
            .get(cur)
            .copied()
            .and_then(|id| self.store.read(id, "rounding", None));
        match mode {
            Some(V::Variant { name, .. }) => match &*name {
                "HalfUp" => RoundingStrategy::MidpointAwayFromZero,
                "Up" => RoundingStrategy::AwayFromZero,
                "Down" => RoundingStrategy::ToZero,
                _ => RoundingStrategy::MidpointNearestEven,
            },
            _ => RoundingStrategy::MidpointNearestEven,
        }
    }

    fn code_of(&mut self, v: &V) -> R<String> {
        match v {
            V::Ref { id, .. } => Ok(self
                .store
                .read(*id, "code", None)
                .map(|c| c.to_string())
                .unwrap_or_default()),
            V::Text(t) => Ok(t.to_string()),
            other => fail(format!("expected a unit or currency, found {other}")),
        }
    }

    /// The zero `z` stands for (`0 EUR`, `0 pcs`, `0 EUR/pcs`), its currency and unit read
    /// from `this` and the locals of `ctx`; `None` when one of them is not known (yet).
    fn typed_zero(
        &mut self,
        z: &quadra_ir::Zero,
        this: Option<&V>,
        ctx: Option<&Ctx>,
    ) -> Option<V> {
        let mut codes = Vec::new();
        for u in &z.units {
            let code = match u {
                UnitRef::Code(c) => c.clone(),
                UnitRef::Path(p) => {
                    let root = p.first()?.as_str();
                    let mut cur = match root {
                        "self" => this.cloned()?,
                        "current" => V::Current,
                        _ => match ctx.and_then(|c| c.local(root)).cloned() {
                            Some(v) => v,
                            None if root == "company" => V::Ref {
                                ent: Rc::from("Company"),
                                id: ctx.and_then(|c| c.company).or(self.company)?,
                            },
                            None => return None,
                        },
                    };
                    for seg in &p[1..] {
                        if !matches!(cur, V::Ref { .. } | V::Rec { .. } | V::Current) {
                            return None;
                        }
                        cur = self.get(&cur, seg, None, None).ok()?;
                    }
                    match cur {
                        V::Ref { .. } | V::Text(_) => {
                            self.code_of(&cur).ok().filter(|c| !c.is_empty())?
                        }
                        _ => return None,
                    }
                }
            };
            codes.push(Rc::<str>::from(code.as_str()));
        }
        Some(match z.kind {
            ZeroKind::Money => V::Money {
                amt: Decimal::ZERO,
                cur: codes[0].clone(),
            },
            ZeroKind::Qty => V::Qty {
                v: Decimal::ZERO,
                unit: codes[0].clone(),
            },
            ZeroKind::Price => V::Price {
                amt: Decimal::ZERO,
                cur: codes[0].clone(),
                per: codes.get(1)?.clone(),
            },
        })
    }

    /// A bare `0` computed for something typed (a field, a `sum`): the zero of that type.
    fn zero_for(&mut self, span: Span, v: V, this: Option<&V>, ctx: Option<&Ctx>) -> V {
        if !is_bare_zero(&v) {
            return v;
        }
        let Some(z) = self.m.zeros.get(&span) else {
            return v;
        };
        self.typed_zero(z, this, ctx).unwrap_or(v)
    }

    fn by_code(&mut self, ent: &str, code: &str) -> Option<V> {
        self.ids(ent, None)
            .into_iter()
            .find(|id| {
                self.store
                    .read(*id, "code", None)
                    .map_or(false, |c| c.to_string() == code)
            })
            .map(|id| V::Ref {
                ent: Rc::from(ent),
                id,
            })
    }

    // ---- reading members --------------------------------------------------------------

    /// `record.field`, including formulas, inverse collections, per-company values,
    /// defaults computed on first read, and frozen snapshots.
    pub fn get(&mut self, rec: &V, name: &str, company: Option<Id>, as_of: Option<u64>) -> R<V> {
        match rec {
            V::Ref { ent, id } => self.get_ref(ent, *id, name, company, as_of),
            V::Rec { fields, .. } => Ok(fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(V::None)),
            V::Tuple(items) => match items.iter().find(|(n, _)| n.as_deref() == Some(name)) {
                Some((_, v)) => Ok(v.clone()),
                None => fail(format!("no field `{name}` in {rec}")),
            },
            V::Current => Ok(match name {
                "user" => self
                    .user
                    .map(|id| V::Ref {
                        ent: Rc::from("User"),
                        id,
                    })
                    .unwrap_or(V::None),
                "company" => self
                    .company
                    .map(|id| V::Ref {
                        ent: Rc::from("Company"),
                        id,
                    })
                    .unwrap_or(V::None),
                _ => V::None,
            }),
            V::Money { amt, cur } => match name {
                "raw" => Ok(V::Dec(*amt)),
                "currency" => Ok(self.by_code("Currency", cur).unwrap_or(V::None)),
                _ => fail(format!("Money has no field `{name}`")),
            },
            V::Price { amt, .. } if name == "raw" => Ok(V::Dec(*amt)),
            V::Qty { v, .. } if name == "raw" => Ok(V::Dec(*v)),
            V::Dec(x) | V::Pct(x) if name == "raw" => Ok(V::Dec(*x)),
            V::Int(i) if name == "raw" => Ok(V::Int(*i)),
            V::Date(dt) => Ok(match name {
                "month" => V::YearMonth(dt.year(), dt.month()),
                "year" => V::Int(dt.year() as i64),
                "weekday" => V::Int(dt.weekday().number_from_monday() as i64),
                _ => V::None,
            }),
            // an instant read in the project's zone
            V::DateTime(dt) => {
                let l = time::local(*dt, self.tz);
                Ok(match name {
                    "date" => V::Date(l.date()),
                    "month" => V::YearMonth(l.year(), l.month()),
                    "year" => V::Int(l.year() as i64),
                    "hour" => V::Int(l.hour() as i64),
                    "minute" => V::Int(l.minute() as i64),
                    "weekday" => V::Int(l.weekday().number_from_monday() as i64),
                    _ => V::None,
                })
            }
            V::Text(_) if name == "is_provisional" => Ok(V::Bool(false)),
            V::None => Ok(V::None),
            other => fail(format!("cannot read `{name}` of {other}")),
        }
    }

    fn get_ref(
        &mut self,
        ent: &str,
        id: Id,
        name: &str,
        company: Option<Id>,
        as_of: Option<u64>,
    ) -> R<V> {
        // a column a caller wrote (`/v1/query`): evaluated as that caller, never with the rights
        // of the model's own formulas
        if name.starts_with("__col_") {
            return self.get_ref_inner(ent, id, name, company, as_of);
        }
        // a record's own formulas read what they read, whoever looks at them
        self.viewer_pause += 1;
        let r = self.get_ref_inner(ent, id, name, company, as_of);
        self.viewer_pause -= 1;
        r
    }

    fn get_ref_inner(
        &mut self,
        ent: &str,
        id: Id,
        name: &str,
        company: Option<Id>,
        as_of: Option<u64>,
    ) -> R<V> {
        let e = self.entity(ent)?;
        if e.is_ledger {
            let Some(row) = self.store.ledger_row(id) else {
                return Ok(V::None);
            };
            return Ok(match name {
                "source" => row.source.clone().unwrap_or(V::None),
                "date" => V::DateTime(row.date),
                _ => row.cols.get(name).cloned().unwrap_or(V::None),
            });
        }
        self.store.load_as(id, ent);
        let actual: Rc<str> = match self.store.records.get(&id) {
            Some(r) => r.ent.clone(),
            None => Rc::from(ent),
        };
        let (e, known) = if *actual == *ent {
            (e, true)
        } else {
            match self.m.entity(&actual) {
                Some(a) => (a, true),
                None => (e, false),
            }
        };
        let Some(f) = known.then(|| self.m.find_field(e, name)).flatten() else {
            let rec = self.store.get(id).cloned();
            let rec = rec.as_ref();
            return Ok(match name {
                "id" => text(&id.to_string()),
                "created_at" => rec.map(|r| V::DateTime(r.created_at)).unwrap_or(V::None),
                "created_by" => rec
                    .and_then(|r| r.created_by)
                    .map(|u| V::Ref {
                        ent: Rc::from("User"),
                        id: u,
                    })
                    .unwrap_or(V::None),
                // the last transaction that wrote the record: when and by whom
                "updated_at" | "updated_by" => {
                    let Some(r) = rec else { return Ok(V::None) };
                    let tx = r.version();
                    let (at, by) = if tx == r.created_tx {
                        (r.created_at, r.created_by)
                    } else if let Some(c) = self.store.commit_info(tx) {
                        c
                    } else if tx == self.store.tx {
                        (self.clock, self.user)
                    } else {
                        (r.cause.time, None)
                    };
                    if name == "updated_at" {
                        V::DateTime(at)
                    } else {
                        by.map(|u| V::Ref {
                            ent: Rc::from("User"),
                            id: u,
                        })
                        .unwrap_or(V::None)
                    }
                }
                "version" => {
                    // the whole history (a record read from its projection row has only its last values)
                    self.store.history_from(id, 0);
                    V::Int(
                        self.store
                            .records
                            .get(&id)
                            .map(|r| r.history.len() as i64)
                            .unwrap_or(0),
                    )
                }
                "warnings" => {
                    let mut out: Vec<V> = rec
                        .map(|r| r.warnings.iter().map(|w| text(w)).collect())
                        .unwrap_or_default();
                    for ce in &self.m.entities {
                        let Some(pf) = ce.fields.iter().find(|f| f.parent) else {
                            continue;
                        };
                        for cid in self.referencing(&ce.name, &pf.name, id) {
                            if let Some(child) = self.store.records.get(&cid) {
                                out.extend(child.warnings.iter().map(|w| text(w)));
                            }
                        }
                    }
                    list(out)
                }
                _ => return fail(format!("`{actual}` has no field `{name}`")),
            });
        };
        let this = V::Ref {
            ent: actual.clone(),
            id,
        };
        let company = if f.per_company {
            company.or(self.company)
        } else {
            None
        };
        if let Some(formula) = &f.formula {
            // before the transaction (a rollup's old contributions): its stored value
            if self.inc.old_view {
                return self.old_formula(&actual, f, id);
            }
            // frozen records, and the lines of a frozen document, read the world as of the
            // moment it was frozen; `live` formulas keep following the data
            let live = e
                .frozen
                .as_ref()
                .map_or(false, |fr| fr.live.iter().any(|l| l == name))
                || e.live_decls.iter().any(|(l, _)| l == name);
            let frozen = if live {
                None
            } else {
                self.frozen_owner(id).map(|(_, t)| t)
            };
            let view = match frozen {
                Some(t) => Some(as_of.map_or(t, |a| a.min(t))),
                None => as_of,
            };
            // an incremental formula read now (or as of a moment at or after the freezing of a
            // frozen record, which reads the world as of then): its stored value
            let current = match (as_of, frozen) {
                (None, _) => true,
                (Some(a), Some(t)) => a >= t,
                (Some(_), None) => false,
            };
            // (a bulk import or a deploy keeps the values it computes: `formula_memo`)
            let trusted = company.is_none()
                && self.store.lazy()
                && self.formula_memo.is_none()
                && self.trusted_field(e, f);
            if trusted && current {
                let settled = frozen.is_some_and(|t| t < self.store.tx);
                if let Some(v) = self.stored_formula(&actual, f, id, settled)? {
                    return Ok(v);
                }
            }
            let memo = view.is_none() && company.is_none() && self.formula_memo.is_some();
            if memo {
                if let Some(v) = self
                    .formula_memo
                    .as_ref()
                    .and_then(|m| m.get(&(id, f.name.as_str())))
                {
                    return Ok(v.clone());
                }
            }
            let mut ctx = Ctx::for_record(&this, &actual);
            ctx.as_of = view;
            ctx.company = company;
            if f.per_company {
                if let Some(c) = company {
                    ctx.bind(
                        "company",
                        V::Ref {
                            ent: Rc::from("Company"),
                            id: c,
                        },
                    );
                }
            }
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                self.depth -= 1;
                return fail(format!("formula recursion too deep in {actual}.{name}"));
            }
            let r = self.eval(formula, &mut ctx);
            self.depth -= 1;
            let r = r.map(|v| self.zero_for(f.span, v, Some(&this), Some(&ctx)));
            if let (true, Ok(v), Some(m)) = (memo, &r, self.formula_memo.as_mut()) {
                m.insert((id, f.name.as_str()), v.clone());
            }
            if let (true, true, Ok(v)) = (trusted, current, &r) {
                self.remember(id, f.name.as_str(), v);
            }
            return r;
        }
        if self.inc.old_view
            && matches!(f.ty, Some(Ty::List(_)) | Some(Ty::Set(_)))
            && inverse_field(self.m, e, f)
        {
            return fail("no collection before the transaction");
        }
        // the snapshot applies to formulas; the record's own stored fields are read as asked
        if let Some(coll) = self.collection(&actual, id, f, as_of)? {
            return Ok(coll);
        }
        let key: std::borrow::Cow<str> = match company {
            Some(c) => std::borrow::Cow::Owned(format!("{name}@{c}")),
            None => std::borrow::Cow::Borrowed(name),
        };
        if self.inc.old_view {
            // the value before the transaction (a field without one reads as below: a set with
            // no value is the empty set, as a record read from its row has it)
            let tx = self.store.tx;
            let rec = self.store.records.get(&id);
            return Ok(match rec.and_then(|r| r.before(tx, &key)) {
                Some(v) => v,
                None if rec.is_some_and(|r| r.created_tx < tx) => match &f.ty {
                    Some(Ty::List(_)) => list(Vec::new()),
                    Some(Ty::Set(_)) => V::Set(Rc::new(Vec::new())),
                    _ => V::None,
                },
                None => V::None,
            });
        }
        if let Some(v) = self.store.read(id, &key, as_of) {
            return Ok(v);
        }
        // default not computed yet
        if self
            .store
            .records
            .get(&id)
            .map_or(false, |r| r.pending_defaults.iter().any(|p| p == name))
        {
            return self.compute_default(&actual, id, name);
        }
        Ok(match &f.ty {
            Some(Ty::List(_)) => list(Vec::new()),
            Some(Ty::Set(_)) => V::Set(Rc::new(Vec::new())),
            _ => V::None,
        })
    }

    fn compute_default(&mut self, ent: &str, id: Id, name: &str) -> R<V> {
        if let Some(r) = self.store.records.get_mut(&id) {
            r.pending_defaults.retain(|p| p != name);
        }
        let Some(e) = self.m.entity(ent) else {
            return Ok(V::None);
        };
        // an override in the entity (`number default …`) wins over the trait's default
        let expr = e
            .fields
            .iter()
            .find(|f| f.name == name && f.default.is_some())
            .and_then(|f| f.default.as_ref())
            .or_else(|| self.m.find_field(e, name).and_then(|f| f.default.as_ref()));
        let Some(expr) = expr else { return Ok(V::None) };
        let per_company = self.m.find_field(e, name).map_or(false, |f| f.per_company);
        let this = V::Ref {
            ent: Rc::from(ent),
            id,
        };
        let mut ctx = Ctx::for_record(&this, ent);
        let v = self.eval(expr, &mut ctx)?;
        let v = match self.m.find_field(e, name) {
            Some(f) => self.zero_for(f.span, v, Some(&this), Some(&ctx)),
            None => v,
        };
        let key = if per_company {
            format!("{name}@{}", self.company.unwrap_or(0))
        } else {
            name.to_string()
        };
        self.store.write(id, &key, v.clone());
        Ok(v)
    }

    /// Records of `ent` (or its implementors) whose `field` may point to `id`: an index
    /// lookup with storage on demand, every record otherwise. Callers still filter.
    fn referencing(&mut self, ent: &str, field: &str, id: Id) -> Vec<Id> {
        let names: Vec<String> = match self.m.entity(ent) {
            Some(e) if e.kind == EntityKind::Trait => self
                .m
                .implementors(ent)
                .iter()
                .map(|e| e.name.clone())
                .collect(),
            _ => vec![ent.to_string()],
        };
        if !self.store.lazy() {
            // in memory: an index of the field, built on first use
            let mut out = Vec::new();
            for n in names {
                out.extend(self.store.referencing_mem(&n, field, id));
            }
            out.sort();
            out.dedup();
            return out;
        }
        let target = V::Ref {
            ent: Rc::from(""),
            id,
        };
        let mut out = Vec::new();
        for n in names {
            match self.store.find(&n, field, &target) {
                Some(ids) => out.extend(ids),
                None => out.extend(self.ids(&n, None)),
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// `Table.where(x => x.col == value and …)` with storage on demand: the column and value
    /// to look up instead of scanning the whole table.
    fn pushdown(&mut self, args: &[Arg], ctx: &mut Ctx) -> Option<(String, V)> {
        if !self.store.lazy() || ctx.as_of.is_some() {
            return None;
        }
        let Some(Arg {
            value:
                Expr {
                    kind: ExprKind::Lambda { params, body },
                    ..
                },
            ..
        }) = args.first()
        else {
            return None;
        };
        let p = params.first()?;
        fn conjuncts(e: &Expr) -> Vec<&Expr> {
            match &e.kind {
                ExprKind::Binary {
                    op: BinOp::And,
                    lhs,
                    rhs,
                } => {
                    let mut v = conjuncts(lhs);
                    v.extend(conjuncts(rhs));
                    v
                }
                _ => vec![e],
            }
        }
        fn mentions(e: &Expr, p: &str) -> bool {
            format!("{:?}", e.kind).contains(&format!("Name(\"{p}\")"))
        }
        for c in conjuncts(body) {
            let ExprKind::Binary {
                op: BinOp::Eq,
                lhs,
                rhs,
            } = &c.kind
            else {
                continue;
            };
            for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                let ExprKind::Field { base, name, .. } = &side.kind else {
                    continue;
                };
                if !matches!(&base.kind, ExprKind::Name(n) if n == p) || mentions(other, p) {
                    continue;
                }
                if let Ok(v) = self.eval(other, ctx) {
                    if !v.is_none() {
                        return Some((name.clone(), v));
                    }
                }
            }
        }
        None
    }

    /// Children / inverse relations computed from the other side.
    fn collection(&mut self, ent: &str, id: Id, f: &Field, as_of: Option<u64>) -> R<Option<V>> {
        let Some(ty) = &f.ty else { return Ok(None) };
        let (inner, set) = match ty {
            Ty::List(i) => (i.as_ref(), false),
            Ty::Set(i) => (i.as_ref(), true),
            _ => return Ok(None),
        };
        let Ty::Path { segments, .. } = inner else {
            return Ok(None);
        };
        let child = segments[0].as_str();
        let Some(ce) = self.m.entity(child) else {
            return Ok(None);
        };
        if ce.kind == EntityKind::Record {
            return Ok(None);
        }
        let owner = self.entity(ent)?;
        if let Some(through) = &f.through {
            // {Partner} through SupplierInfo: SupplierInfo rows of this record → their Partner
            let Some(te) = self.m.entity(through) else {
                return Ok(None);
            };
            let back = te
                .fields
                .iter()
                .find(|x| ty_names(x.ty.as_ref()) == Some(owner.name.clone()))
                .map(|x| x.name.clone());
            let fwd = te
                .fields
                .iter()
                .find(|x| ty_names(x.ty.as_ref()).as_deref() == Some(child))
                .map(|x| x.name.clone());
            let (Some(back), Some(fwd)) = (back, fwd) else {
                return Ok(None);
            };
            let mut out = Vec::new();
            let candidates = if as_of.is_none() {
                self.referencing(through, &back, id)
            } else {
                self.ids(through, as_of)
            };
            for tid in candidates {
                if self
                    .store
                    .read(tid, &back, as_of)
                    .and_then(|v| v.as_ref_id())
                    == Some(id)
                {
                    if let Some(v) = self.store.read(tid, &fwd, as_of) {
                        if !out.contains(&v) {
                            out.push(v);
                        }
                    }
                }
            }
            return Ok(Some(V::Set(Rc::new(out))));
        }
        let back = f.via.clone().or_else(|| {
            ce.fields
                .iter()
                .find(|x| {
                    let n = ty_names(x.ty.as_ref());
                    n.as_deref() == Some(owner.name.as_str())
                        || n.map_or(false, |n| owner.traits.contains(&n))
                })
                .map(|x| x.name.clone())
        });
        let Some(back) = back else { return Ok(None) };
        // many-to-many inverse: `partners: {Partner} via tags` where tags is a set
        let back_is_set = self
            .field(child, &back)
            .and_then(|bf| bf.ty.as_ref())
            .map_or(false, |t| matches!(t, Ty::Set(_)));
        let back_is_parent = self.field(child, &back).map_or(false, |bf| bf.parent);
        // the records pointing to this one now are those that pointed to it then, when nothing
        // could move since: the lines of a document frozen at or before `as_of` (a frozen
        // document's lines change only with it); otherwise every record is a candidate
        let now_is_then = match as_of {
            None => true,
            Some(t) => {
                back_is_parent
                    && self
                        .frozen_owner(id)
                        .map_or(false, |(_, frozen)| frozen <= t)
            }
        };
        let mut out = Vec::new();
        let candidates = if now_is_then {
            self.referencing(child, &back, id)
        } else {
            self.ids(child, as_of)
        };
        for cid in candidates {
            let v = self.store.read(cid, &back, as_of);
            let hit = match &v {
                Some(V::Ref { id: x, .. }) => *x == id,
                Some(V::Set(items)) | Some(V::List(items)) if back_is_set => {
                    items.iter().any(|i| i.as_ref_id() == Some(id))
                }
                _ => false,
            };
            if hit {
                out.push(V::Ref {
                    ent: self.ent_of(cid),
                    id: cid,
                });
            }
        }
        Ok(Some(if set { V::Set(Rc::new(out)) } else { list(out) }))
    }

    // ---- writing ----------------------------------------------------------------------

    pub fn set(&mut self, rec: &V, name: &str, v: V, company: Option<Id>) -> R<()> {
        let V::Ref { ent, id } = rec else {
            return fail(format!("cannot assign `{name}` on {rec}"));
        };
        let actual = self.ent_of(*id);
        let e = self.entity(&actual)?;
        let Some(f) = self.m.find_field(e, name) else {
            return fail(format!("`{ent}` has no field `{name}`"));
        };
        if f.formula.is_some() {
            return fail(format!("`{name}` is a formula: it cannot be assigned"));
        }
        if self.script == 0 {
            if let Some(msg) = self.m.flow_assignment_error(e, name) {
                return fail(msg);
            }
        }
        match self.frozen_owner(*id) {
            // `except` fields still change; the field of a flow only in a migration or change
            Some((owner, _)) if owner == *id => {
                if let Some(fr) = &e.frozen {
                    if !fr.except.iter().any(|x| x == name)
                        && !e.flows.iter().any(|fl| fl.field == name)
                    {
                        return fail(format!(
                            "{actual} is frozen ({}): `{name}` cannot change",
                            print::expr(&fr.when)
                        ));
                    }
                }
            }
            // the lines of a frozen document are frozen with it
            Some((owner, _)) => {
                let pe = self.ent_of(owner);
                if let Some(fr) = self.m.entity(&pe).and_then(|x| x.frozen.as_ref()) {
                    return fail(format!(
                        "{pe} is frozen ({}): its {actual} lines cannot change",
                        print::expr(&fr.when)
                    ));
                }
            }
            None => {}
        }
        self.check_permission("edit", &actual, Some(name), Some(rec))?;
        self.write_field(&actual, *id, f, v, company)?;
        // what the commit follows from a write: up the references (rollups, checks), loaded
        // level by level in a few queries
        if self.store.lazy() {
            self.store.prefetch_refs(&[*id], 3);
        }
        // a change setting the state of a flow back out of the frozen states unfreezes now
        if e.flows.iter().any(|fl| fl.field == name)
            && self
                .store
                .records
                .get(id)
                .map_or(false, |r| r.frozen_at.is_some())
            && !self.frozen_holds(*id, None)?
        {
            self.thaw(*id);
        }
        Ok(())
    }

    /// The record whose freezing `id` follows, with the transaction it was frozen at: the
    /// record itself, or the document it is a line of (up its `parent` fields). None when
    /// it is not frozen.
    pub fn frozen_owner(&mut self, id: Id) -> Option<(Id, u64)> {
        let mut cur = id;
        for _ in 0..10 {
            if let Some(t) = self.store.get(cur).and_then(|r| r.frozen_at) {
                return Some((cur, t));
            }
            let ce = self.ent_of(cur);
            let pf = self
                .m
                .entity(&ce)
                .and_then(|x| x.fields.iter().find(|f| f.parent))?;
            cur = self
                .store
                .read(cur, &pf.name, None)
                .and_then(|v| v.as_ref_id())?;
        }
        None
    }

    /// Does `frozen when` hold on a frozen record, read on the current data (not its
    /// snapshot), with the field of a flow possibly set to the state a transition is about to
    /// give it? The record is left as it was.
    fn frozen_holds(&mut self, id: Id, state: Option<(&str, V)>) -> R<bool> {
        let ent = self.ent_of(id);
        let Some(e) = self.m.entity(&ent) else {
            return Ok(false);
        };
        let Some(fr) = &e.frozen else {
            return Ok(false);
        };
        let Some(r) = self.store.records.get_mut(&id) else {
            return Ok(false);
        };
        let frozen_at = r.frozen_at.take();
        // what is computed meanwhile is for this moment only
        self.store.epoch += 1;
        let before = state
            .as_ref()
            .map(|(f, v)| (f.to_string(), r.fields.insert(f.to_string(), v.clone())));
        let rec = V::Ref {
            ent: ent.clone(),
            id,
        };
        let holds = self
            .eval(&fr.when, &mut Ctx::for_record(&rec, &ent))
            .map(|v| v.truthy());
        self.store.epoch += 1;
        if let Some(r) = self.store.records.get_mut(&id) {
            r.frozen_at = frozen_at;
            if let Some((f, old)) = before {
                match old {
                    Some(v) => r.fields.insert(f, v),
                    None => r.fields.remove(&f),
                };
            }
        }
        holds
    }

    /// The record is no longer frozen: `frozen when` stopped holding. Its history is kept.
    fn thaw(&mut self, id: Id) {
        if let Some(r) = self.store.records.get_mut(&id) {
            r.frozen_at = None;
            self.store.epoch += 1;
            self.store.thawed.insert(id);
            self.store.dirty.insert(id);
        }
        // the document and its lines read the current world again: for what depends on them,
        // as if every field of theirs were written
        if self.store.lazy() && !self.store.untracked {
            let thawed = std::mem::replace(
                &mut self.store.thawed,
                std::collections::BTreeSet::from([id]),
            );
            let family = self.thawed_family();
            self.store.thawed = thawed;
            for x in family {
                self.store.wlog.push((x, Rc::from("")));
            }
        }
    }

    /// The records unfrozen in this transaction and the lines of those documents (up to ten
    /// levels of lines).
    fn thawed_family(&mut self) -> Vec<Id> {
        let lines: Vec<(String, String)> = self
            .m
            .entities
            .iter()
            .filter_map(|e| {
                e.fields
                    .iter()
                    .find(|f| f.parent)
                    .map(|f| (e.name.clone(), f.name.clone()))
            })
            .collect();
        let mut family: Vec<Id> = self.store.thawed.iter().copied().collect();
        let mut next = family.clone();
        for _ in 0..10 {
            let mut found = Vec::new();
            for doc in &next {
                for (le, pf) in &lines {
                    for cid in self.referencing(le, pf, *doc) {
                        if self.store.read(cid, pf, None).and_then(|v| v.as_ref_id()) == Some(*doc)
                            && !family.contains(&cid)
                            && !found.contains(&cid)
                        {
                            found.push(cid);
                        }
                    }
                }
            }
            if found.is_empty() {
                break;
            }
            family.extend(found.iter().copied());
            next = found;
        }
        family
    }

    /// Records of `ent` unfrozen in this transaction, and the lines of those documents: their
    /// rules and formulas apply to the current data again. With storage on demand they are
    /// not reached from what was written, so they are added to the records to re-evaluate.
    pub fn thawed_of(&mut self, ent: &str) -> Vec<Id> {
        if self.store.thawed.is_empty() || !self.store.lazy() {
            return Vec::new();
        }
        let family = self.thawed_family();
        family
            .into_iter()
            .filter(|id| {
                let e = self.ent_of(*id);
                self.is_a(&e, ent)
            })
            .collect()
    }

    fn write_field(&mut self, ent: &str, id: Id, f: &Field, v: V, company: Option<Id>) -> R<()> {
        let key = if f.per_company {
            format!("{}@{}", f.name, company.or(self.company).unwrap_or(0))
        } else {
            f.name.clone()
        };
        let creating = self.store.records.get(&id).map_or(false, |r| {
            r.created_tx == self.store.tx && Rc::ptr_eq(&r.cause, &self.cause)
        });
        if let Some(r) = self.store.records.get_mut(&id) {
            r.pending_defaults.retain(|p| p != &f.name);
        }
        if let (Some(r), false) = (self.store.records.get_mut(&id), creating) {
            r.cause = Rc::new(Cause {
                desc: format!("{ent}.{} changed by {}", f.name, self.cause.desc),
                user: self.cause.user.clone(),
                time: self.clock,
                parent: self.cause.parent.clone(),
            });
        }
        // children given as a collection: point their back-reference to this record
        if let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty {
            if let (Some(child), None) = (ty_names(Some(inner)), &f.through) {
                if let Some(ce) = self
                    .m
                    .entity(&child)
                    .filter(|ce| ce.kind != EntityKind::Record)
                {
                    let owner = self.entity(ent)?;
                    let back = f.via.clone().or_else(|| {
                        ce.fields
                            .iter()
                            .find(|x| {
                                let n = ty_names(x.ty.as_ref());
                                n.as_deref() == Some(owner.name.as_str())
                                    || n.map_or(false, |n| owner.traits.contains(&n))
                            })
                            .map(|x| x.name.clone())
                    });
                    let back_is_set = back
                        .as_ref()
                        .and_then(|b| self.field(&child, b))
                        .and_then(|bf| bf.ty.as_ref())
                        .map_or(false, |t| matches!(t, Ty::Set(_)));
                    if let (Some(back), false) = (back, back_is_set) {
                        for item in v.items().unwrap_or(&[]).to_vec() {
                            if let V::Ref { id: cid, .. } = item {
                                self.store.write(
                                    cid,
                                    &back,
                                    V::Ref {
                                        ent: Rc::from(ent),
                                        id,
                                    },
                                );
                            }
                        }
                        return Ok(());
                    }
                }
            }
        }
        let v = match self.m.find_field(self.entity(ent)?, &f.name) {
            Some(tf) => self.zero_for(
                tf.span,
                v,
                Some(&V::Ref {
                    ent: Rc::from(ent),
                    id,
                }),
                None,
            ),
            None => v,
        };
        self.store.write(id, &key, v);
        // a real value replaces a provisional one
        let marker = format!("{key}#provisional");
        if self
            .store
            .read(id, &marker, None)
            .map_or(false, |m| !m.is_none())
        {
            self.store.write(id, &marker, V::None);
        }
        Ok(())
    }

    /// `Entity(field=value, …)`: a new record (or a value, for `record` types).
    pub fn create(&mut self, ent: &str, args: Vec<(String, V)>) -> R<V> {
        let e = self.entity(ent)?;
        if ent == "User" {
            if let Some(max) = crate::limits::current().max_users {
                if self.ids("User", None).len() as u64 >= max {
                    crate::limits::hit(crate::limits::too_many_users(max));
                    return fail(crate::limits::too_many_users(max));
                }
            }
        }
        if self.viewing() && e.kind != EntityKind::Record {
            return fail(format!(
                "{ent}(…): a record cannot be created in a formula of an app (it only reads)"
            ));
        }
        if e.kind == EntityKind::Record {
            // in the order of the declaration, whatever the order of the arguments: two equal
            // values are equal (`Address(city=…, zip=…) == Address(zip=…, city=…)`)
            let mut fields: Vec<(String, V)> = Vec::new();
            for f in self.m.all_fields(e) {
                let v = match args.iter().find(|(n, _)| n == &f.name) {
                    Some((_, v)) => v.clone(),
                    None => match &f.default {
                        Some(d) => self.eval(d, &mut Ctx::default())?,
                        None => V::None,
                    },
                };
                fields.push((f.name.clone(), v));
            }
            for (n, v) in &args {
                if !fields.iter().any(|(x, _)| x == n) {
                    fields.push((n.clone(), v.clone()));
                }
            }
            return Ok(V::Rec {
                ent: Rc::from(ent),
                fields: Rc::new(fields),
            });
        }
        if e.kind == EntityKind::Trait {
            return fail(format!(
                "`{ent}` is a trait: create one of its entities instead"
            ));
        }
        let id = self.store.new_id();
        let pending: Vec<String> = self
            .m
            .all_fields(e)
            .into_iter()
            .chain(e.fields.iter().filter(|f| f.default_override))
            .filter(|f| f.default.is_some() && !args.iter().any(|(n, _)| n == &f.name))
            .map(|f| f.name.clone())
            .collect();
        self.store.insert(Record {
            ent: Rc::from(ent),
            id,
            fields: Default::default(),
            history: Vec::new(),
            created_tx: self.store.tx,
            created_at: self.clock,
            created_by: self.user,
            pending_defaults: pending,
            frozen_at: None,
            cause: self.cause.clone(),
            warnings: Vec::new(),
            partial: None,
            formulas: HashMap::new(),
        });
        // a bare `0` takes its currency or unit from the record: written after the others
        let mut args = args;
        args.sort_by_key(|(_, v)| is_bare_zero(v));
        for (name, v) in args {
            let Some(f) = self.m.find_field(e, &name) else {
                return fail(format!("`{ent}` has no field `{name}`"));
            };
            if f.formula.is_some() {
                return fail(format!("`{name}` is a formula: it cannot be set"));
            }
            // a new record starts in the initial state of its flows
            if let (0, Some(initial), V::Variant { name: st, .. }) =
                (self.script, self.m.initial_state(e, &name), &v)
            {
                if **st != *initial {
                    return fail(format!("`{ent}.{name}` changes only through its flow: a new {ent} starts in {initial}"));
                }
            }
            self.write_field(ent, id, f, v, None)?;
        }
        if self.store.lazy() {
            self.store.prefetch_refs(&[id], 3);
        }
        Ok(V::Ref {
            ent: Rc::from(ent),
            id,
        })
    }

    /// Compute every pending default (end of statement).
    pub fn flush_defaults(&mut self) -> R<()> {
        loop {
            let pending: Vec<(Id, String, String)> = self
                .store
                .records
                .values()
                .flat_map(|r| {
                    r.pending_defaults
                        .iter()
                        .map(move |p| (r.id, r.ent.to_string(), p.clone()))
                })
                .collect();
            if pending.is_empty() {
                return Ok(());
            }
            for (id, ent, name) in pending {
                if self
                    .store
                    .records
                    .get(&id)
                    .map_or(false, |r| r.pending_defaults.contains(&name))
                {
                    self.compute_default(&ent, id, &name)?;
                }
            }
        }
    }

    // ---- permissions ------------------------------------------------------------------

    /// The roles of a user, with the roles they inherit (`role Senior is Clerk`).
    pub fn roles_of(&mut self, user: Id) -> Vec<String> {
        let mut out = Vec::new();
        self.store.load_as(user, "User");
        if let Some(V::Set(rs)) | Some(V::List(rs)) = self.store.read(user, "roles", None) {
            for r in rs.iter() {
                if let V::Role(name) | V::Variant { name, .. } = r {
                    let mut cur = Some(name.to_string());
                    while let Some(n) = cur {
                        if out.contains(&n) {
                            break;
                        }
                        cur = self.m.role(&n).and_then(|r| r.parent.clone());
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    fn rule_matches(rule: &Rule, verb: &str, ent: &str, member: Option<&str>) -> bool {
        let verb_ok = rule
            .verbs
            .iter()
            .any(|v| v == verb || v == "all" || (verb == "create" && v == "edit" && false));
        verb_ok
            && rule.targets.iter().any(|t| match t.as_slice() {
                [s] if s == "*" => true,
                [e] => e == ent,
                [e, m, ..] => e == ent && Some(m.as_str()) == member,
                _ => false,
            })
    }

    fn rule_condition(&mut self, rule: &Rule, rec: Option<&V>) -> R<bool> {
        let Some(cond) = &rule.condition else {
            return Ok(true);
        };
        let Some(rec) = rec else { return Ok(true) };
        let saved = self.system;
        self.system += 1;
        let r = match &cond.kind {
            ExprKind::Lambda { params, body } => {
                let mut ctx = Ctx::default();
                ctx.bind(
                    params.first().map(String::as_str).unwrap_or("_"),
                    rec.clone(),
                );
                self.eval(body, &mut ctx).map(|v| v.truthy())
            }
            _ => Ok(true),
        };
        self.system = saved;
        r
    }

    /// Permission check for user-initiated operations (not inside actions or reactions).
    /// Check a stored amount against its declared currency or unit (values coming from outside
    /// the language: the API). Quantities in another unit of the same kind are converted;
    /// a different currency or kind of unit is refused.
    pub fn coerce_field(&mut self, rec: &V, field: &str) -> R<()> {
        let V::Ref { id, .. } = rec else {
            return Ok(());
        };
        let ent = self.ent_of(*id);
        let Some(f) = self.field(&ent, field) else {
            return Ok(());
        };
        let ty = match &f.ty {
            Some(Ty::Optional(t)) => t.as_ref(),
            Some(t) => t,
            None => return Ok(()),
        };
        let Ty::Path { segments, args, .. } = ty else {
            return Ok(());
        };
        let kind = segments.last().map(String::as_str).unwrap_or("");
        if !matches!(kind, "Money" | "Qty" | "Price") {
            return Ok(());
        }
        let v = self.get(rec, field, None, None)?;
        if v.is_none() {
            return Ok(());
        }
        let expected = self.expected_units(rec, args);
        let first = expected.first().cloned().flatten();
        // a bare number takes the record's currency or unit
        if let (Some(n), Some(e)) = (
            match &v {
                V::Dec(d) => Some(*d),
                V::Int(i) => Some(Decimal::from(*i)),
                _ => None,
            },
            first.clone(),
        ) {
            let nv = match kind {
                "Money" => V::Money {
                    amt: n,
                    cur: Rc::from(e.as_str()),
                },
                "Qty" => V::Qty {
                    v: n,
                    unit: Rc::from(e.as_str()),
                },
                _ => match expected.get(1).cloned().flatten() {
                    Some(per) => V::Price {
                        amt: n,
                        cur: Rc::from(e.as_str()),
                        per: Rc::from(per.as_str()),
                    },
                    None => {
                        return fail(format!(
                            "{ent}.{field}: write the price with its unit, e.g. 10 {e}/pcs"
                        ))
                    }
                },
            };
            let company = self.company;
            return self.set(rec, field, nv, company);
        }
        let fixed = match (&v, first) {
            (V::Money { cur, .. }, Some(e)) if **cur != *e => {
                return fail(format!(
                    "{ent}.{field}: expected an amount in {e}, not {cur}"
                ))
            }
            (V::Price { cur, .. }, Some(e)) if **cur != *e => {
                return fail(format!("{ent}.{field}: expected a price in {e}, not {cur}"))
            }
            (V::Price { per, .. }, _)
                if expected
                    .get(1)
                    .cloned()
                    .flatten()
                    .map_or(false, |u| *u != **per) =>
            {
                return fail(format!(
                    "{ent}.{field}: expected a price per {}, not per {per}",
                    expected[1].clone().unwrap()
                ));
            }
            (V::Qty { v: q, unit }, Some(e)) if **unit != *e => {
                match self.convert_qty(*q, unit, &e) {
                    Ok(c) => Some(V::Qty {
                        v: c,
                        unit: Rc::from(e.as_str()),
                    }),
                    Err(_) => {
                        return fail(format!(
                            "{ent}.{field}: expected a quantity in {e}, not {unit}"
                        ))
                    }
                }
            }
            _ => None,
        };
        if let Some(nv) = fixed {
            let company = self.company;
            self.set(rec, field, nv, company)?;
        }
        Ok(())
    }

    /// The currency / units a `Money<…>`, `Qty<…>` or `Price<…>` field of `rec` expects: a
    /// code, or a path from the record (`uom`, `order.currency`); None when not known.
    fn expected_units(&mut self, rec: &V, args: &[Ty]) -> Vec<Option<String>> {
        let mut expected = Vec::new();
        for a in args {
            let Ty::Path { segments: p, .. } = a else {
                expected.push(None);
                continue;
            };
            if p.len() == 1
                && (self.currencies.contains_key(&p[0]) || self.units.contains_key(&p[0]))
            {
                expected.push(Some(p[0].clone()));
                continue;
            }
            let mut cur = if p.first().map(String::as_str) == Some("current") {
                None
            } else {
                Some(rec.clone())
            };
            for seg in p.iter().filter(|s| *s != "self") {
                cur = match cur {
                    Some(c @ V::Ref { .. }) => {
                        self.get(&c, seg, None, None).ok().filter(|v| !v.is_none())
                    }
                    _ => None,
                };
            }
            expected.push(match cur {
                Some(c @ V::Ref { .. }) => self.code_of(&c).ok(),
                Some(V::Text(t)) => Some(t.to_string()),
                _ => None,
            });
        }
        expected
    }

    /// A quantity written in another unit of the same kind (`500 g` in a `Qty<kg>` field) is
    /// stored in the field's unit, as the API stores it. Runs on the records written in the
    /// transaction, once their defaults are known.
    fn store_in_field_units(&mut self) -> R<()> {
        for id in self.store.dirty.iter().copied().collect::<Vec<_>>() {
            let ent = self.ent_of(id);
            let Some(e) = self.m.entity(&ent) else {
                continue;
            };
            for f in self.m.all_fields(e) {
                if f.per_company || f.formula.is_some() {
                    continue;
                }
                let ty = match &f.ty {
                    Some(Ty::Optional(t)) => t.as_ref(),
                    Some(t) => t,
                    None => continue,
                };
                let Ty::Path { segments, args, .. } = ty else {
                    continue;
                };
                if segments.last().map(String::as_str) != Some("Qty") {
                    continue;
                }
                let Some(V::Qty { v, unit }) = self.store.read(id, &f.name, None) else {
                    continue;
                };
                let rec = V::Ref {
                    ent: ent.clone(),
                    id,
                };
                let Some(Some(to)) = self.expected_units(&rec, args).into_iter().next() else {
                    continue;
                };
                if *unit == *to {
                    continue;
                }
                // another kind of unit is refused by its own check
                if let Ok(c) = self.convert_qty(v, &unit, &to) {
                    self.store.write(
                        id,
                        &f.name,
                        V::Qty {
                            v: c,
                            unit: Rc::from(to.as_str()),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// `check_permission` as a yes/no (for the API: what a user may see and do).
    pub fn can(&mut self, verb: &str, ent: &str, member: Option<&str>, rec: Option<&V>) -> bool {
        self.check_permission(verb, ent, member, rec).is_ok()
    }

    /// A field hidden from the current user: a `hide` rule of one of their roles covers it
    /// and no `show` rule of any of their roles gives it back.
    pub fn hidden(&mut self, ent: &str, field: &str) -> bool {
        let Some(user) = self.user else { return false };
        if self.system > 0 {
            return false;
        }
        let roles = self.roles_of(user);
        let traits: Vec<String> = self
            .m
            .entity(ent)
            .map(|e| {
                self.m
                    .all_traits(e)
                    .iter()
                    .map(|t| t.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let covers = |rule: &Rule| {
            rule.targets.iter().any(|t| match t.as_slice() {
                [e, f, ..] => (e == ent || traits.contains(e)) && f == field,
                _ => false,
            })
        };
        let rules: Vec<&Rule> = roles
            .iter()
            .filter_map(|r| self.m.role(r))
            .flat_map(|r| r.rules.iter())
            .collect();
        rules.iter().any(|r| r.effect == "hide" && covers(r))
            && !rules.iter().any(|r| r.effect == "show" && covers(r))
    }

    /// `read history`: versions and causes of records of `ent`.
    pub fn can_read_history(&mut self, ent: &str) -> bool {
        let Some(user) = self.user else { return true };
        let roles = self.roles_of(user);
        let grants = |rule: &Rule| {
            (rule.history || rule.verbs.iter().any(|v| v == "all"))
                && Self::rule_matches(rule, "read", ent, None)
        };
        let rules: Vec<&Rule> = roles
            .iter()
            .filter_map(|r| self.m.role(r))
            .flat_map(|r| r.rules.iter())
            .chain(self.m.rules.iter())
            .collect();
        if rules
            .iter()
            .any(|r| r.effect == "deny" && r.history && Self::rule_matches(r, "read", ent, None))
        {
            return false;
        }
        rules.iter().any(|r| r.effect == "allow" && grants(r))
    }

    pub fn check_permission(
        &mut self,
        verb: &str,
        ent: &str,
        member: Option<&str>,
        rec: Option<&V>,
    ) -> R<()> {
        self.viewer_pause += 1;
        let r = self.check_permission_inner(verb, ent, member, rec);
        self.viewer_pause -= 1;
        r
    }

    fn check_permission_inner(
        &mut self,
        verb: &str,
        ent: &str,
        member: Option<&str>,
        rec: Option<&V>,
    ) -> R<()> {
        let Some(user) = self.user else { return Ok(()) };
        if self.system > 0 {
            return Ok(());
        }
        let login = self.user_name();
        let roles = self.roles_of(user);
        // `deny` rules: those at the top level apply to everybody, those of a role to its
        // users (and to the users of the roles that inherit it); they win over any `allow`
        let m = self.m;
        let own: Vec<&Rule> = roles
            .iter()
            .filter_map(|r| m.role(r))
            .flat_map(|r| r.rules.iter())
            .collect();
        for rule in m.rules.iter().chain(own.iter().copied()) {
            // (`deny read history X` refuses only the history: see `can_read_history`)
            // (without a record — may the user ever do this? — a `deny … where` depends on the
            // record: it is checked on each one, it does not refuse the whole entity)
            if rule.effect == "deny"
                && !rule.history
                && Self::rule_matches(rule, verb, ent, member)
                && (rec.is_some() || rule.condition.is_none())
                && self.rule_condition(rule, rec)?
            {
                let target = member
                    .map(|m| format!("{ent}.{m}"))
                    .unwrap_or(ent.to_string());
                return fail(format!("permission denied: deny {verb} {target} ({login})"));
            }
        }
        // a model without roles does not model permissions yet: everything is allowed (the
        // `deny` rules above still apply). Declaring a role turns the rules on.
        if m.roles.is_empty() {
            return Ok(());
        }
        // `allow` rules: of the user's roles, and those at the top level (for everybody)
        for rule in own.iter().copied().chain(m.rules.iter()) {
            if rule.effect != "allow" {
                continue;
            }
            if Self::rule_matches(rule, verb, ent, member) && self.rule_condition(rule, rec)? {
                return Ok(());
            }
            // `read, create, edit SalesOrder` also allows editing its fields
            if verb == "edit"
                && member.is_some()
                && Self::rule_matches(rule, "edit", ent, None)
                && self.rule_condition(rule, rec)?
            {
                return Ok(());
            }
        }
        // lines of a document (`parent` field) follow the document: reading or changing a
        // line is reading or changing its document
        if let Some(pf) = self
            .m
            .entity(ent)
            .and_then(|e| e.fields.iter().find(|f| f.parent))
        {
            let parent_ent = ty_names(pf.ty.as_ref()).unwrap_or_default();
            let parent = match rec {
                Some(V::Ref { id, .. }) => self
                    .store
                    .read(*id, &pf.name, None)
                    .filter(|v| !v.is_none()),
                _ => None,
            };
            let pverb = match verb {
                "read" => "read",
                "edit" | "create" => "edit",
                _ => "",
            };
            if !pverb.is_empty() && !parent_ent.is_empty() && (parent.is_some() || rec.is_none()) {
                if self
                    .check_permission(pverb, &parent_ent, None, parent.as_ref())
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }
        let target = member
            .map(|m| format!("{ent}.{m}"))
            .unwrap_or(ent.to_string());
        fail(format!("permission denied: {login} cannot {verb} {target}"))
    }

    // ---- expressions ------------------------------------------------------------------

    pub fn eval(&mut self, e: &Expr, ctx: &mut Ctx) -> R<V> {
        crate::limits::count_step();
        if let Some(d) = self.deadline {
            self.ticks = self.ticks.wrapping_add(1);
            if self.ticks % 1024 == 0 && std::time::Instant::now() > d {
                // past the job's own deadline (not only a shorter one): the job answers with an error
                if crate::limits::expired() {
                    crate::limits::hit(crate::limits::TIMEOUT);
                }
                return fail("timeout: the evaluation took too long (simplify the formula or filter the rows first)");
            }
        }
        match self.max_list {
            None => self.eval_kind(e, ctx),
            Some(max) => {
                let v = self.eval_kind(e, ctx)?;
                self.check_len(&v, max)?;
                Ok(v)
            }
        }
    }

    /// A list or a set beyond the job's cap stops the evaluation.
    fn check_len(&self, v: &V, max: usize) -> R<()> {
        if let V::List(x) | V::Set(x) = v {
            if x.len() > max {
                let m = crate::limits::too_long_list(max);
                crate::limits::hit(m.clone());
                return fail(m);
            }
        }
        Ok(())
    }

    fn eval_kind(&mut self, e: &Expr, ctx: &mut Ctx) -> R<V> {
        match &e.kind {
            ExprKind::Lit(l) => Ok(match lit(l) {
                // a date-time written in the code is a wall-clock time of the project's zone
                V::DateTime(d) => V::DateTime(time::instant(d, self.tz)),
                v => v,
            }),
            ExprKind::FString(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        FPart::Text(t) => s.push_str(t),
                        FPart::Expr(x, spec) => {
                            let v = self.eval(x, ctx).unwrap_or(V::None);
                            match spec {
                                Some(spec) => {
                                    let shown = self.format_value(&v, spec);
                                    s.push_str(&shown);
                                }
                                None => s.push_str(&self.display(&v)),
                            }
                        }
                    }
                }
                Ok(text(&s))
            }
            ExprKind::Quantity { value, unit, per } => {
                let v = parse_dec(value);
                if let Some(secs) = quadra_types::duration_unit(unit) {
                    return Ok(V::Duration((v * d(secs)).trunc().to_i64().unwrap_or(0)));
                }
                if let Some(p) = per {
                    return Ok(V::Price {
                        amt: v,
                        cur: Rc::from(unit.as_str()),
                        per: Rc::from(p.as_str()),
                    });
                }
                // ISO 4217 codes are three capital letters: no unit of measure looks like that,
                // and seeds run before the currency table is loaded
                let iso = unit.len() == 3 && unit.bytes().all(|b| b.is_ascii_uppercase());
                if self.currencies.contains_key(unit) || (iso && !self.units.contains_key(unit)) {
                    Ok(V::Money {
                        amt: v,
                        cur: Rc::from(unit.as_str()),
                    })
                } else {
                    Ok(V::Qty {
                        v,
                        unit: Rc::from(unit.as_str()),
                    })
                }
            }
            ExprKind::Name(n) => self.name(n, ctx),
            ExprKind::SelfRef => Ok(ctx.this.clone().unwrap_or(V::None)),
            ExprKind::Field { base, name, .. }
                if matches!(&base.kind, ExprKind::Name(b) if b == "env")
                    && ctx.local("env").is_none()
                    && self.m.manifest.var(name).is_some() =>
            {
                self.env_value(name)
            }
            ExprKind::Field {
                base,
                name,
                optional,
            } => {
                // `record.field.is_provisional`
                if name == "is_provisional" {
                    if let ExprKind::Field {
                        base: rb,
                        name: field,
                        ..
                    } = &base.kind
                    {
                        if let V::Ref { id, .. } = self.eval(rb, ctx)? {
                            let m = self
                                .store
                                .read(id, &format!("{field}#provisional"), ctx.as_of);
                            return Ok(V::Bool(m.map_or(false, |m| !m.is_none())));
                        }
                    }
                }
                let b = self.eval(base, ctx)?;
                if b.is_none() {
                    return if *optional {
                        Ok(V::None)
                    } else {
                        fail(format!(
                            "`{}` is none: cannot read `{name}`",
                            print::expr(base)
                        ))
                    };
                }
                if self.viewing() {
                    if let V::Ref { ent, id } = &b {
                        self.viewer_read(ent, *id, name)?;
                    }
                }
                self.get(&b, name, None, ctx.as_of)
            }
            ExprKind::Call { callee, args } => {
                let v = self.call(callee, args, ctx)?;
                let this = ctx.this.clone();
                Ok(self.zero_for(e.span, v, this.as_ref(), Some(ctx)))
            }
            ExprKind::Index { base, index } => {
                // per-company member at a company: `product.cost[company]`
                if let ExprKind::Field {
                    base: recv, name, ..
                } = &base.kind
                {
                    let r = self.eval(recv, ctx)?;
                    if let V::Ref { ent, .. } = &r {
                        if self.field(ent, name).map_or(false, |f| f.per_company) {
                            let c = self.eval(index, ctx)?.as_ref_id();
                            return self.get(&r, name, c, ctx.as_of);
                        }
                    }
                }
                let b = self.eval(base, ctx)?;
                let i = self.eval(index, ctx)?;
                match (&b, &i) {
                    (V::List(v) | V::Set(v), V::Int(i)) => {
                        Ok(v.get(*i as usize).cloned().unwrap_or(V::None))
                    }
                    _ => fail(format!("cannot index {b}")),
                }
            }
            ExprKind::Unary { op, expr } => {
                let v = self.eval(expr, ctx)?;
                match op {
                    UnOp::Not => Ok(V::Bool(!v.truthy())),
                    UnOp::Neg => neg(v),
                }
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, ctx),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                if self.eval(cond, ctx)?.truthy() {
                    self.eval(then, ctx)
                } else {
                    self.eval(otherwise, ctx)
                }
            }
            ExprKind::Lambda { params, body } => Ok(V::Lambda(Rc::new(Closure {
                params: params.clone(),
                body: body.clone(),
                locals: ctx
                    .scopes
                    .iter()
                    .flat_map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())))
                    .collect(),
                this: ctx.this.clone(),
                entity: ctx.entity.clone(),
                company: ctx.company,
                as_of: ctx.as_of,
            }))),
            ExprKind::Tuple(items) => {
                let mut out = Vec::new();
                for (n, x) in items {
                    out.push((n.clone(), self.eval(x, ctx)?));
                }
                Ok(V::Tuple(Rc::new(out)))
            }
            ExprKind::List(items) => {
                let mut out = Vec::new();
                for a in items {
                    let v = self.eval(&a.value, ctx)?;
                    if a.spread {
                        out.extend(v.items().unwrap_or(&[]).iter().cloned());
                    } else {
                        out.push(v);
                    }
                }
                Ok(list(out))
            }
            ExprKind::Set(items) => {
                let mut out: Vec<V> = Vec::new();
                for x in items {
                    let v = self.eval(x, ctx)?;
                    if !out.contains(&v) {
                        out.push(v);
                    }
                }
                Ok(V::Set(Rc::new(out)))
            }
            ExprKind::When(arms) => {
                for (c, v) in arms {
                    match c {
                        Some(c) if !self.eval(c, ctx)?.truthy() => continue,
                        _ => return self.eval(v, ctx),
                    }
                }
                Ok(V::None)
            }
            ExprKind::Match { subject, arms } => {
                let s = self.eval(subject, ctx)?;
                for arm in arms {
                    for p in &arm.patterns {
                        let mut binds = Vec::new();
                        if self.pattern(p, &s, &mut binds) {
                            ctx.scopes.push(binds.into_iter().collect());
                            let r = self.eval(&arm.body, ctx);
                            ctx.scopes.pop();
                            return r;
                        }
                    }
                }
                fail(format!("no match arm for {s}"))
            }
            ExprKind::Block(stmts) => {
                ctx.scopes.push(Default::default());
                let r = self.block_value(stmts, ctx);
                ctx.scopes.pop();
                r
            }
            ExprKind::As { user, body } => {
                if self.viewer {
                    return fail("`as` is not allowed here: an expression of an app runs as whoever looks at it");
                }
                let saved = self.user;
                self.user = self.login_id(user);
                let r = self.eval(body, ctx);
                self.user = saved;
                r
            }
            ExprKind::Fail(x) => {
                let v = self.eval(x, ctx)?;
                fail(v.to_string())
            }
            ExprKind::Error => fail("expression with errors"),
        }
    }

    /// A value formatted by an f-string spec (`{n:05}`, `{x:.2}`, `{name:<10}`).
    pub fn format_value(&mut self, v: &V, spec: &FormatSpec) -> String {
        let fixed = |d: Decimal, p: u32| {
            let r = d.round_dp_with_strategy(p, RoundingStrategy::MidpointNearestEven);
            format!("{:.*}", p as usize, r)
        };
        let (shown, numeric) = match (v, spec.precision) {
            (V::Int(i), Some(p)) => (fixed(Decimal::from(*i), p), true),
            (V::Dec(d), Some(p)) => (fixed(*d, p), true),
            (V::Pct(d), Some(p)) => (format!("{}%", fixed(d * Decimal::from(100), p)), true),
            (V::Money { amt, cur }, Some(p)) => (format!("{} {cur}", fixed(*amt, p)), true),
            (V::Price { amt, cur, per }, Some(p)) => {
                (format!("{} {cur}/{per}", fixed(*amt, p)), true)
            }
            (V::Qty { v, unit }, Some(p)) => (format!("{} {unit}", fixed(*v, p)), true),
            (
                V::Int(_)
                | V::Dec(_)
                | V::Pct(_)
                | V::Money { .. }
                | V::Price { .. }
                | V::Qty { .. },
                None,
            ) => (self.display(v), true),
            _ => (self.display(v), false),
        };
        spec.pad(
            &shown,
            numeric && matches!(v, V::Int(_) | V::Dec(_)) || (numeric && !spec.zero),
        )
    }

    pub fn display(&mut self, v: &V) -> String {
        match v {
            // an instant, as the clock of the project's zone shows it
            V::DateTime(dt) => time::local(*dt, self.tz)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            V::Ref { ent, id } => {
                for key in ["number", "code", "name", "login"] {
                    if let Some(x) = self.store.read(*id, key, None) {
                        if !x.is_none() {
                            return x.to_string();
                        }
                    }
                }
                format!("{ent}#{id}")
            }
            // a value record (an address): its parts, as a person would write them
            V::Rec { ent, fields } => {
                // in the order the record declares its fields
                let order: Vec<String> = self
                    .m
                    .entity(ent)
                    .map(|e| e.fields.iter().map(|f| f.name.clone()).collect())
                    .unwrap_or_default();
                let mut items: Vec<(usize, V)> = fields
                    .iter()
                    .filter(|(_, v)| !v.is_none())
                    .map(|(n, v)| {
                        (
                            order.iter().position(|o| o == n).unwrap_or(usize::MAX),
                            v.clone(),
                        )
                    })
                    .collect();
                items.sort_by_key(|(i, _)| *i);
                let parts: Vec<String> = items
                    .iter()
                    .map(|(_, v)| self.display(v))
                    .filter(|s| !s.is_empty())
                    .take(4)
                    .collect();
                parts.join(", ")
            }
            V::List(items) | V::Set(items) => {
                let shown: Vec<String> = items
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|v| self.display(v))
                    .collect();
                if items.len() > 3 {
                    format!("{} … ({})", shown.join(", "), items.len())
                } else {
                    shown.join(", ")
                }
            }
            V::None => String::new(),
            // amounts and prices with the decimals of their currency
            V::Money { amt, cur } => format!("{:.*} {cur}", self.decimals(cur) as usize, amt),
            V::Price { amt, cur, per } => format!(
                "{} {cur}/{per}",
                crate::value::price_amount(amt, self.decimals(cur))
            ),
            other => other.to_string(),
        }
    }

    fn login_id(&mut self, login: &str) -> Option<Id> {
        self.ids("User", None).into_iter().find(|id| {
            self.store
                .read(*id, "login", None)
                .map_or(false, |v| v.to_string() == login)
        })
    }

    fn pattern(&mut self, p: &Pattern, v: &V, binds: &mut Vec<(String, V)>) -> bool {
        match p {
            Pattern::Wildcard => true,
            Pattern::Lit(Lit::None) => v.is_none(),
            Pattern::Lit(l) => lit(l) == *v,
            Pattern::Name(n) => {
                if n.chars().next().map_or(false, char::is_uppercase) {
                    match v {
                        V::Variant { name, .. } => **name == **n,
                        V::Ref { ent, .. } => {
                            **ent == **n
                                || self
                                    .m
                                    .entity(ent)
                                    .map_or(false, |e| self.m.implements(e, n))
                        }
                        _ => false,
                    }
                } else {
                    binds.push((n.clone(), v.clone()));
                    true
                }
            }
            Pattern::Ctor { name, args } => {
                if name == "some" {
                    if v.is_none() {
                        return false;
                    }
                    return args.first().map_or(true, |a| self.pattern(a, v, binds));
                }
                match v {
                    V::Variant { name: vn, fields } if **vn == **name => args
                        .iter()
                        .zip(fields.iter())
                        .all(|(a, f)| self.pattern(a, f, binds)),
                    V::Ref { ent, .. }
                        if **ent == **name
                            || self
                                .m
                                .entity(ent)
                                .map_or(false, |e| self.m.implements(e, name)) =>
                    {
                        args.first().map_or(true, |a| self.pattern(a, v, binds))
                    }
                    _ => false,
                }
            }
        }
    }

    fn name(&mut self, n: &str, ctx: &mut Ctx) -> R<V> {
        if let Some(v) = ctx.local(n) {
            return Ok(v.clone());
        }
        match n {
            "me" => {
                return Ok(self
                    .user
                    .map(|id| V::Ref {
                        ent: Rc::from("User"),
                        id,
                    })
                    .unwrap_or(V::None))
            }
            "current" => return Ok(V::Current),
            _ => {}
        }
        if let (Some(this), Some(ent)) = (ctx.this.clone(), ctx.entity.clone()) {
            if let V::Ref { .. } | V::Rec { .. } = this {
                let known = self.field(&ent, n).is_some()
                    || [
                        "id",
                        "created_at",
                        "created_by",
                        "updated_at",
                        "updated_by",
                        "version",
                    ]
                    .contains(&n);
                if known {
                    if self.viewing() {
                        if let V::Ref { ent, id } = &this {
                            self.viewer_read(ent, *id, n)?;
                        }
                    }
                    return self.get(&this, n, ctx.company, ctx.as_of);
                }
            }
        }
        if self.m.entity(n).is_some() {
            return Ok(V::Table(Rc::from(n)));
        }
        if self.m.variant(n).is_some() {
            return Ok(V::Variant {
                name: Rc::from(n),
                fields: Rc::new(Vec::new()),
            });
        }
        if let Some(c) = self.m.constant(n) {
            let v = c.value.clone();
            return self.eval(&v, &mut Ctx::default());
        }
        if let Some(v) = self
            .by_code("Currency", n)
            .or_else(|| self.by_code("Uom", n))
        {
            return Ok(v);
        }
        if self.m.role(n).is_some() {
            return Ok(V::Role(Rc::from(n)));
        }
        fail(format!("unknown name `{n}`"))
    }

    // ---- operators --------------------------------------------------------------------

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, ctx: &mut Ctx) -> R<V> {
        use BinOp::*;
        match op {
            And => {
                let l = self.eval(lhs, ctx)?;
                if !l.truthy() {
                    return Ok(V::Bool(false));
                }
                return Ok(V::Bool(self.eval(rhs, ctx)?.truthy()));
            }
            Or => {
                let l = self.eval(lhs, ctx)?;
                if l.truthy() {
                    return Ok(V::Bool(true));
                }
                return Ok(V::Bool(self.eval(rhs, ctx)?.truthy()));
            }
            Implies => {
                let l = self.eval(lhs, ctx)?;
                if !l.truthy() {
                    return Ok(V::Bool(true));
                }
                return Ok(V::Bool(self.eval(rhs, ctx)?.truthy()));
            }
            Coalesce => {
                let l = self.eval(lhs, ctx)?;
                return if l.is_none() {
                    self.eval(rhs, ctx)
                } else {
                    Ok(l)
                };
            }
            _ => {}
        }
        let l = self.eval(lhs, ctx)?;
        // an instant plus days or weeks written out (`+ 1 day`): days of the calendar in the
        // project's zone, the same wall-clock time across a change of the clocks; any other
        // duration (`+ 24 hours`, a computed one) is exact time
        if let (
            V::DateTime(dt),
            Add | Sub,
            ExprKind::Quantity {
                value,
                unit,
                per: None,
            },
        ) = (&l, op, &rhs.kind)
        {
            let days = match unit.as_str() {
                "day" | "days" => Some(1),
                "week" | "weeks" => Some(7),
                _ => None,
            };
            if let Some(k) = days {
                let n = parse_dec(value);
                if n.fract().is_zero() {
                    let n = n.to_i64().unwrap_or(0) * k * if op == Add { 1 } else { -1 };
                    return Ok(V::DateTime(time::add_days(*dt, n, self.tz)));
                }
            }
        }
        let r = self.eval(rhs, ctx)?;
        self.binop(op, l, r)
    }

    /// The zone a value names (the text of an IANA name); an empty one is the project's.
    fn zone_of(&self, v: &V) -> R<chrono_tz::Tz> {
        match v {
            V::Text(t) if t.is_empty() => Ok(self.tz),
            V::Text(t) => time::zone(t).or_else(|e| fail(e)),
            V::None => Ok(self.tz),
            other => fail(format!(
                "a time zone is the text of its name (`Europe/Rome`), not {other}"
            )),
        }
    }

    /// `dt.in(tz)`: the instant and what it is in the zone (std `ZonedTime`).
    fn zoned(&self, dt: NaiveDateTime, tz: chrono_tz::Tz) -> V {
        let l = time::local(dt, tz);
        V::Rec {
            ent: Rc::from("ZonedTime"),
            fields: Rc::new(vec![
                ("instant".into(), V::DateTime(dt)),
                ("timezone".into(), text(tz.name())),
                ("date".into(), V::Date(l.date())),
                ("hour".into(), V::Int(l.hour() as i64)),
                ("minute".into(), V::Int(l.minute() as i64)),
                (
                    "weekday".into(),
                    V::Int(l.weekday().number_from_monday() as i64),
                ),
                ("offset".into(), V::Duration(time::offset(dt, tz))),
            ]),
        }
    }

    pub fn binop(&self, op: BinOp, l: V, r: V) -> R<V> {
        use BinOp::*;
        Ok(match op {
            Eq => V::Bool(self.equal(&l, &r)?),
            Ne => V::Bool(!self.equal(&l, &r)?),
            Lt | Le | Gt | Ge => {
                let c = self.compare(&l, &r)?;
                V::Bool(match op {
                    Lt => c < 0,
                    Le => c <= 0,
                    Gt => c > 0,
                    _ => c >= 0,
                })
            }
            In | NotIn => {
                let hit = match &r {
                    V::List(v) | V::Set(v) => v.iter().any(|x| self.equal(&l, x).unwrap_or(false)),
                    V::DateRange(a, b) => match &l {
                        V::Date(d) => a <= d && d <= b,
                        V::DateTime(dt) => {
                            let d = time::local(*dt, self.tz).date();
                            *a <= d && d <= *b
                        }
                        _ => false,
                    },
                    _ => false,
                };
                V::Bool(if op == In { hit } else { !hit })
            }
            Is | IsNot => {
                let hit = match (&l, &r) {
                    (V::Variant { name: a, .. }, V::Variant { name: b, .. }) => a == b,
                    (V::Ref { ent, .. }, V::Table(t)) => {
                        **ent == **t
                            || self
                                .m
                                .entity(ent)
                                .map_or(false, |e| self.m.implements(e, t))
                    }
                    (V::None, V::None) => true,
                    (V::None, _) | (_, V::None) => false,
                    (a, b) => a == b,
                };
                V::Bool(if op == Is { hit } else { !hit })
            }
            Range => {
                let (Some(a), Some(b)) = (l.num(), r.num()) else {
                    return fail("ranges need numbers");
                };
                let mut out = Vec::new();
                if let (V::Int(a), V::Int(b)) = (&l, &r) {
                    // counted before it is made: a range of billions is refused, not allocated
                    let n = b
                        .checked_sub(*a)
                        .and_then(|d| d.checked_add(1))
                        .unwrap_or(i64::MAX)
                        .max(0) as u64;
                    let cap = self.max_list.unwrap_or(MAX_VALUES).min(MAX_VALUES) as u64;
                    if n > cap {
                        let m = crate::limits::too_long_list(cap as usize);
                        crate::limits::hit(m.clone());
                        return fail(m);
                    }
                    for i in *a..=*b {
                        out.push(V::Int(i));
                    }
                } else {
                    out.push(l.clone());
                    out.push(r.clone());
                    let _ = (a, b);
                }
                list(out)
            }
            Add | Sub => self.add_sub(op == Add, l, r)?,
            Mul => self.mul(l, r)?,
            Div => self.div(l, r)?,
            _ => V::None,
        })
    }

    fn equal(&self, l: &V, r: &V) -> R<bool> {
        Ok(match (l, r) {
            (V::Qty { v: a, unit: u }, V::Qty { v: b, unit: w }) if u != w => {
                self.convert_qty(*b, w, u).map(|b| *a == b).unwrap_or(false)
            }
            (V::Duration(a), x) | (x, V::Duration(a)) if x.num().is_some_and(|n| n.is_zero()) => {
                *a == 0
            }
            _ => l == r,
        })
    }

    fn compare(&self, l: &V, r: &V) -> R<i32> {
        let ord = |a: Decimal, b: Decimal| {
            if a < b {
                -1
            } else if a > b {
                1
            } else {
                0
            }
        };
        Ok(match (l, r) {
            (V::Money { amt: a, cur: c }, V::Money { amt: b, cur: e }) => {
                if c != e {
                    return fail(format!("cannot compare {l} and {r}: different currencies"));
                }
                ord(*a, *b)
            }
            (V::Money { amt, .. }, x) if x.num().is_some() => ord(*amt, x.num().unwrap()),
            (x, V::Money { amt, .. }) if x.num().is_some() => ord(x.num().unwrap(), *amt),
            (V::Qty { v: a, unit: u }, V::Qty { v: b, unit: w }) => {
                ord(*a, self.convert_qty(*b, w, u)?)
            }
            (V::Qty { v, .. }, x) if x.num().is_some() => ord(*v, x.num().unwrap()),
            (x, V::Qty { v, .. }) if x.num().is_some() => ord(x.num().unwrap(), *v),
            (V::Price { amt: a, cur: c, .. }, V::Price { amt: b, cur: e, .. }) => {
                if c != e {
                    return fail(format!("cannot compare {l} and {r}: different currencies"));
                }
                ord(*a, *b)
            }
            (V::Price { amt, .. }, x) if x.num().is_some() => ord(*amt, x.num().unwrap()),
            (x, V::Price { amt, .. }) if x.num().is_some() => ord(x.num().unwrap(), *amt),
            (V::Date(a), V::Date(b)) => a.cmp(b) as i32,
            (V::DateTime(a), V::DateTime(b)) => a.cmp(b) as i32,
            (V::Date(a), V::DateTime(b)) => a.cmp(&time::local(*b, self.tz).date()) as i32,
            (V::Duration(a), V::Duration(b)) => a.cmp(b) as i32,
            // `hours > 0`: zero is zero in every unit
            (V::Duration(a), x) if x.num().is_some_and(|n| n.is_zero()) => {
                ord(Decimal::from(*a), Decimal::ZERO)
            }
            (x, V::Duration(b)) if x.num().is_some_and(|n| n.is_zero()) => {
                ord(Decimal::ZERO, Decimal::from(*b))
            }
            (V::Text(a), V::Text(b)) => a.cmp(b) as i32,
            (V::Tuple(a), V::Tuple(b)) => {
                for ((_, x), (_, y)) in a.iter().zip(b.iter()) {
                    let c = self.compare(x, y)?;
                    if c != 0 {
                        return Ok(c);
                    }
                }
                0
            }
            (a, b) => match (a.num(), b.num()) {
                (Some(x), Some(y)) => ord(x, y),
                _ => return fail(format!("cannot compare {l} and {r}")),
            },
        })
    }

    fn add_sub(&self, add: bool, l: V, r: V) -> R<V> {
        let s = |a: Decimal, b: Decimal| if add { a + b } else { a - b };
        Ok(match (&l, &r) {
            (V::Money { amt: a, cur: c }, V::Money { amt: b, cur: e }) => {
                if c != e {
                    return fail(format!("cannot add {l} and {r}: different currencies"));
                }
                V::Money {
                    amt: s(*a, *b),
                    cur: c.clone(),
                }
            }
            (V::Money { amt, cur }, V::Int(0)) | (V::Int(0), V::Money { amt, cur }) => {
                let v = if matches!(l, V::Int(0)) && !add {
                    -*amt
                } else {
                    *amt
                };
                V::Money {
                    amt: v,
                    cur: cur.clone(),
                }
            }
            (V::Qty { v: a, unit: u }, V::Qty { v: b, unit: w }) => V::Qty {
                v: s(*a, self.convert_qty(*b, w, u)?),
                unit: u.clone(),
            },
            (V::Qty { v, unit }, V::Int(0)) => V::Qty {
                v: *v,
                unit: unit.clone(),
            },
            (V::Int(0), V::Qty { v, unit }) => V::Qty {
                v: if add { *v } else { -*v },
                unit: unit.clone(),
            },
            (V::Price { amt: a, cur, per }, V::Price { amt: b, .. }) => V::Price {
                amt: s(*a, *b),
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Price { amt, cur, per }, V::Int(0)) => V::Price {
                amt: *amt,
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Int(0), V::Price { amt, cur, per }) => V::Price {
                amt: if add { *amt } else { -*amt },
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Date(dt), V::Duration(secs)) => {
                V::Date(*dt + Duration::seconds(if add { *secs } else { -*secs }))
            }
            (V::DateTime(dt), V::Duration(secs)) => {
                V::DateTime(*dt + Duration::seconds(if add { *secs } else { -*secs }))
            }
            (V::Date(a), V::Date(b)) if !add => V::Duration((*a - *b).num_seconds()),
            (V::DateTime(a), V::DateTime(b)) if !add => V::Duration((*a - *b).num_seconds()),
            (V::Duration(a), V::Duration(b)) => V::Duration(if add { a + b } else { a - b }),
            (V::Duration(a), x) if x.num().is_some_and(|n| n.is_zero()) => V::Duration(*a),
            (x, V::Duration(b)) if x.num().is_some_and(|n| n.is_zero()) => {
                V::Duration(if add { *b } else { -*b })
            }
            (V::Text(a), V::Text(b)) if add => {
                if a.len() + b.len() > MAX_TEXT {
                    return fail(format!("a text longer than {} MB", MAX_TEXT >> 20));
                }
                text(&format!("{a}{b}"))
            }
            (V::List(a) | V::Set(a), V::List(b) | V::Set(b)) if add => {
                let mut out = a.to_vec();
                for x in b.iter() {
                    if matches!(l, V::List(_)) || !out.contains(x) {
                        out.push(x.clone());
                    }
                }
                if matches!(l, V::Set(_)) {
                    V::Set(Rc::new(out))
                } else {
                    list(out)
                }
            }
            (V::Int(a), V::Int(b)) => match if add {
                a.checked_add(*b)
            } else {
                a.checked_sub(*b)
            } {
                Some(n) => V::Int(n),
                None => return fail("an integer too large: the result does not fit"),
            },
            (V::Pct(a), V::Pct(b)) => V::Pct(s(*a, *b)),
            (a, b) => match (a.num(), b.num()) {
                (Some(x), Some(y)) => {
                    // 1 - 10% = 0.9
                    V::Dec(s(x, y))
                }
                _ => {
                    return fail(format!(
                        "cannot {} {l} and {r}",
                        if add { "add" } else { "subtract" }
                    ))
                }
            },
        })
    }

    fn mul(&self, l: V, r: V) -> R<V> {
        Ok(match (&l, &r) {
            (V::Qty { v, unit }, V::Price { amt, cur, per })
            | (V::Price { amt, cur, per }, V::Qty { v, unit }) => {
                // multiply before dividing: 10 min × 60 €/h = 10 × 60 × (1/60) exactly
                let (f1, f2) = self.factors(unit, per)?;
                V::Money {
                    amt: tidy(*v * amt * f1 / f2),
                    cur: cur.clone(),
                }
            }
            (V::Money { amt, cur }, x) | (x, V::Money { amt, cur }) if x.num().is_some() => {
                V::Money {
                    amt: amt * x.num().unwrap(),
                    cur: cur.clone(),
                }
            }
            (V::Price { amt, cur, per }, x) | (x, V::Price { amt, cur, per })
                if x.num().is_some() =>
            {
                V::Price {
                    amt: amt * x.num().unwrap(),
                    cur: cur.clone(),
                    per: per.clone(),
                }
            }
            (V::Qty { v, unit }, x) | (x, V::Qty { v, unit }) if x.num().is_some() => V::Qty {
                v: v * x.num().unwrap(),
                unit: unit.clone(),
            },
            (V::Duration(s), x) if x.num().is_some() => {
                V::Duration((d(*s) * x.num().unwrap()).to_i64().unwrap_or(0))
            }
            (V::Int(a), V::Int(b)) => match a.checked_mul(*b) {
                Some(n) => V::Int(n),
                None => return fail("an integer too large: the result does not fit"),
            },
            (V::Pct(a), V::Pct(b)) => V::Pct(a * b),
            (a, b) => match (a.num(), b.num()) {
                (Some(x), Some(y)) => V::Dec(x * y),
                _ => return fail(format!("cannot multiply {l} by {r}")),
            },
        })
    }

    fn div(&self, l: V, r: V) -> R<V> {
        let nz = |x: Decimal| {
            if x.is_zero() {
                fail("division by zero")
            } else {
                Ok(x)
            }
        };
        Ok(match (&l, &r) {
            (V::Money { amt: a, .. }, V::Money { amt: b, .. })
            | (V::Price { amt: a, .. }, V::Price { amt: b, .. }) => V::Dec(a / nz(*b)?),
            (V::Money { amt, cur }, V::Qty { v, unit }) => V::Price {
                amt: tidy(amt / nz(*v)?),
                cur: cur.clone(),
                per: unit.clone(),
            },
            (V::Qty { v: a, unit: u }, V::Qty { v: b, unit: w }) => {
                V::Dec(a / nz(self.convert_qty(*b, w, u)?)?)
            }
            (V::Money { amt, cur }, x) if x.num().is_some() => V::Money {
                amt: amt / nz(x.num().unwrap())?,
                cur: cur.clone(),
            },
            (V::Price { amt, cur, per }, x) if x.num().is_some() => V::Price {
                amt: amt / nz(x.num().unwrap())?,
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Qty { v, unit }, x) if x.num().is_some() => V::Qty {
                v: v / nz(x.num().unwrap())?,
                unit: unit.clone(),
            },
            (a, b) => match (a.num(), b.num()) {
                (Some(x), Some(y)) => V::Dec(x / nz(y)?),
                _ => return fail(format!("cannot divide {l} by {r}")),
            },
        })
    }

    // ---- calls ------------------------------------------------------------------------

    fn eval_args(&mut self, args: &[Arg], ctx: &mut Ctx) -> R<(Vec<V>, Vec<(String, V)>)> {
        let mut pos = Vec::new();
        let mut named = Vec::new();
        for a in args {
            let v = self.eval(&a.value, ctx)?;
            match &a.name {
                Some(n) => named.push((n.clone(), v)),
                None => pos.push(v),
            }
        }
        Ok((pos, named))
    }

    fn call(&mut self, callee: &Expr, args: &[Arg], ctx: &mut Ctx) -> R<V> {
        match &callee.kind {
            ExprKind::Field {
                base,
                name,
                optional,
            } => {
                // `ShopOrder.parse(text)`, `PosRow.parse_rows(text)`: a document read into a
                // record `from` a format, a file into its rows
                if name == "parse" || name == "parse_rows" {
                    if let ExprKind::Name(r) = &base.kind {
                        if ctx.local(r).is_none()
                            && self.m.entity(r).is_some_and(|e| e.format.is_some())
                        {
                            let Some(a) = args.first() else {
                                return fail(format!("`{r}.{name}` takes the text to read"));
                            };
                            let text = match self.eval(&a.value, ctx)? {
                                V::Text(t) => t,
                                other => {
                                    return fail(format!("`{r}.{name}` reads text, not {other}"))
                                }
                            };
                            let read = if name == "parse" {
                                crate::formats::parse(self.m, r, &text)
                            } else {
                                crate::formats::parse_rows(self.m, r, &text)
                            };
                            return read.or_else(|e| fail(format!("{r}: {e}")));
                        }
                    }
                }
                // a method over a whole entity (`Order.where(…).count()`): SQL when it can
                if let Some(v) = self.query_call(base, name, args, ctx)? {
                    return Ok(v);
                }
                let b = self.eval(base, ctx)?;
                // `order.json()`, `invoice.xml()`: a record `from` a format written as its
                // document
                if name == "json" || name == "xml" {
                    if let V::Rec { ent, .. } = &b {
                        if self.m.entity(ent).is_some_and(|e| e.format.is_some()) {
                            return crate::formats::render(self.m, &b)
                                .map(|(t, _)| text(&t))
                                .or_else(fail);
                        }
                    }
                }
                // `rows.csv()`, `rows.fixed()`: the file of a list of rows (the checker knows
                // their record, which an empty list does not show)
                if name == "csv" || name == "fixed" {
                    if let V::List(items) = &b {
                        let rec = self.m.row_records.get(&callee.span).cloned().or_else(|| {
                            items.iter().find_map(|r| match r {
                                V::Rec { ent, .. } => Some(ent.to_string()),
                                _ => None,
                            })
                        });
                        if let Some(rec) = rec {
                            return crate::formats::render_rows(self.m, &rec, items)
                                .map(|t| text(&t))
                                .or_else(|e| fail(format!("{rec}: {e}")));
                        }
                    }
                }
                if b.is_none() {
                    return if *optional {
                        Ok(V::None)
                    } else {
                        fail(format!(
                            "`{}` is none: cannot call `{name}`",
                            print::expr(base)
                        ))
                    };
                }
                self.method(b, name, args, ctx)
            }
            ExprKind::Name(n) => self.call_name(n, args, ctx),
            _ => {
                let f = self.eval(callee, ctx)?;
                let (pos, _) = self.eval_args(args, ctx)?;
                self.apply(&f, pos)
            }
        }
    }

    pub fn apply(&mut self, f: &V, args: Vec<V>) -> R<V> {
        let V::Lambda(c) = f else {
            return fail(format!("{f} is not a function"));
        };
        let mut scope: Scope =
            Scope::with_capacity_and_hasher(c.locals.len() + c.params.len(), Default::default());
        scope.extend(c.locals.iter().cloned());
        let mut ctx = Ctx {
            this: c.this.clone(),
            entity: c.entity.clone(),
            scopes: vec![scope],
            company: c.company,
            as_of: c.as_of,
        };
        for (p, a) in c.params.iter().zip(args) {
            ctx.bind(p, a);
        }
        let c = c.clone();
        self.enter("a function passed as a value")?;
        let r = if self.viewer {
            self.viewer_in_query += 1;
            let r = self.eval(&c.body, &mut ctx);
            self.viewer_in_query -= 1;
            r
        } else {
            self.eval(&c.body, &mut ctx)
        };
        self.depth -= 1;
        r
    }

    /// One more call deep (formulas, functions, actions, lambdas): past `MAX_DEPTH` the
    /// evaluation fails, long before the thread's stack would run out (an overflow cannot be
    /// caught: it would end the process, and every tenant in it). `self.depth -= 1` after.
    fn enter(&mut self, what: &str) -> R<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return fail(format!(
                "recursion too deep: more than {MAX_DEPTH} nested calls ({what})"
            ));
        }
        Ok(())
    }

    fn call_fn(
        &mut self,
        f: &FnDef,
        args: &[Arg],
        ctx: &mut Ctx,
        this: Option<V>,
        entity: Option<String>,
    ) -> R<V> {
        let (pos, named) = self.eval_args(args, ctx)?;
        self.invoke_fn(f, pos, named, this, entity)
    }

    fn invoke_fn(
        &mut self,
        f: &FnDef,
        pos: Vec<V>,
        named: Vec<(String, V)>,
        this: Option<V>,
        entity: Option<String>,
    ) -> R<V> {
        let mut fctx = Ctx {
            this,
            entity,
            scopes: vec![Default::default()],
            company: None,
            as_of: None,
        };
        for (k, v) in &named {
            if let Some(name) = k.strip_prefix("__captured_") {
                fctx.bind(name, v.clone());
            }
        }
        let mut pos = pos.into_iter();
        for p in &f.params {
            let v = match named.iter().find(|(n, _)| n == &p.name) {
                Some((_, v)) => v.clone(),
                None => match pos.next() {
                    Some(v) => v,
                    None => match &p.default {
                        Some(dx) => self.eval(dx, &mut fctx.clone())?,
                        None => V::None,
                    },
                },
            };
            fctx.bind(&p.name, v);
        }
        if f.builtin {
            return self.builtin_fn(&f.name, &fctx);
        }
        let Some(body) = &f.body else {
            return Ok(V::None);
        };
        self.enter(&format!("function `{}`", f.name))?;
        let r = match self.stmts(body, &mut fctx) {
            Ok(()) => Ok(V::None),
            Err(Flow::Return(v)) => Ok(v),
            Err(e) => Err(e),
        };
        self.depth -= 1;
        r
    }

    fn builtin_fn(&mut self, name: &str, ctx: &Ctx) -> R<V> {
        match name {
            "next_number" => {
                let company = ctx
                    .local("company")
                    .and_then(|v| v.as_ref_id())
                    .unwrap_or(0);
                let code = ctx.local("code").map(|v| v.to_string()).unwrap_or_default();
                let on = match ctx.local("on") {
                    Some(V::Date(d)) => *d,
                    _ => time::local(self.clock, self.tz).date(),
                };
                let seq = self.ids("Sequence", None).into_iter().find(|id| {
                    self.store
                        .read(*id, "code", None)
                        .map_or(false, |v| v.to_string() == code)
                        && self
                            .store
                            .read(*id, "company", None)
                            .and_then(|v| v.as_ref_id())
                            == Some(company)
                });
                let pattern = seq
                    .and_then(|id| self.store.read(id, "pattern", None))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{code}/{year}/{n:05}".into());
                let key = format!("{company}/{code}/{}", on.year());
                let n = self.store.bump_counter(&key);
                Ok(text(&sequence_number(&pattern, &code, on.year(), n)))
            }
            other => fail(format!("unknown built-in `{other}`")),
        }
    }

    /// The value of a block: its last expression. The block of an `outbound` may end with an
    /// `if … else …` whose branches each end with the request: its value is the branch's.
    fn block_value(&mut self, stmts: &[Stmt], ctx: &mut Ctx) -> R<V> {
        let mut last = V::None;
        for (i, s) in stmts.iter().enumerate() {
            if i + 1 == stmts.len() {
                match &s.kind {
                    StmtKind::Expr(x) => {
                        last = self.eval(x, ctx)?;
                        continue;
                    }
                    StmtKind::If {
                        binding,
                        cond,
                        then,
                        otherwise: Some(otherwise),
                    } if self.building_request => {
                        let v = self.eval(cond, ctx)?;
                        let go = if binding.is_some() {
                            !v.is_none()
                        } else {
                            v.truthy()
                        };
                        ctx.scopes.push(Default::default());
                        if go {
                            if let Some(b) = binding {
                                ctx.bind(b, v);
                            }
                        }
                        let r = self.block_value(if go { then } else { otherwise }, ctx);
                        ctx.scopes.pop();
                        return r;
                    }
                    _ => {}
                }
            }
            self.stmt(s, ctx)?;
        }
        Ok(last)
    }

    fn call_name(&mut self, n: &str, args: &[Arg], ctx: &mut Ctx) -> R<V> {
        if let Some(f) = ctx.local(n).cloned() {
            let (pos, _) = self.eval_args(args, ctx)?;
            return self.apply(&f, pos);
        }
        // signatures of documents, by a signer of quadra.toml
        if matches!(n, "sign" | "p7m" | "xades" | "pgp_sign" | "pgp_encrypt")
            && !self.m.fns.iter().any(|f| f.name == n)
            && ctx.local(n).is_none()
        {
            let (pos, _) = self.eval_args(args, ctx)?;
            return self.sign_call(n, pos);
        }
        // `render(InvoicePdf(inv))`, `zpl(ParcelLabel(p))`: a document of a `.qdp` file
        if matches!(n, "render" | "zpl") && !self.m.fns.iter().any(|f| f.name == n) {
            if let [Arg {
                value:
                    Expr {
                        kind:
                            ExprKind::Call {
                                callee,
                                args: dargs,
                            },
                        ..
                    },
                ..
            }] = args
            {
                if let ExprKind::Name(d) = &callee.kind {
                    let (pos, _) = self.eval_args(dargs, ctx)?;
                    return self.render_document(d, pos, n == "zpl");
                }
            }
            return fail(format!(
                "`{n}` takes a document with its arguments: `{n}(InvoicePdf(inv))`"
            ));
        }
        // the request an `outbound … as e:` block ends with
        if self.building_request
            && matches!(
                n,
                "post" | "put" | "patch" | "delete" | "email" | "write" | "insert" | "print"
            )
        {
            let (pos, named) = self.eval_args(args, ctx)?;
            return self.request(n, pos, named);
        }
        if let Some((f, captured)) = self.local_fns.get(n).cloned() {
            let (pos, named) = self.eval_args(args, ctx)?;
            let mut named = named;
            // captured variables first, so parameters shadow them
            let mut seeded: Vec<(String, V)> = captured;
            seeded.retain(|(k, _)| !f.params.iter().any(|p| &p.name == k));
            named.extend(
                seeded
                    .into_iter()
                    .map(|(k, v)| (format!("__captured_{k}"), v)),
            );
            return self.invoke_fn(&f, pos, named, ctx.this.clone(), ctx.entity.clone());
        }
        match n {
            "today" => {
                let tz = match args.first() {
                    Some(a) => {
                        let v = self.eval(&a.value, ctx)?;
                        self.zone_of(&v)?
                    }
                    None => self.tz,
                };
                return Ok(V::Date(time::local(self.clock, tz).date()));
            }
            "datetime" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                let date = match pos.first() {
                    Some(V::Date(d)) => *d,
                    Some(V::None) | None => return Ok(V::None),
                    Some(other) => return fail(format!("datetime() needs a date, not {other}")),
                };
                let at = match pos.get(1) {
                    Some(V::Text(t)) => match time::time_of_day(t) {
                        Some(x) => x,
                        None => return fail(format!("`{t}` is not a time of day (`09:00`)")),
                    },
                    Some(V::None) | None => return Ok(V::None),
                    Some(other) => {
                        return fail(format!("datetime() needs a time of day, not {other}"))
                    }
                };
                let tz = match pos.get(2) {
                    Some(v) => self.zone_of(v)?,
                    None => self.tz,
                };
                return Ok(V::DateTime(time::instant(date.and_time(at), tz)));
            }
            "now" => return Ok(V::DateTime(self.clock)),
            // `publish … to conn`: whether this change came from an inbound of `conn` (what
            // it received is not sent back to it)
            "__from" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                let conn = pos.first().map(|v| v.to_string()).unwrap_or_default();
                // what an inbound of it received, or a reaction to the outcome of a delivery to
                // it wrote (the id the other side gave the record)
                let names: Vec<String> = self
                    .m
                    .inbounds
                    .iter()
                    .filter(|i| i.connection == conn)
                    .map(|i| format!("inbound:{}#", i.name))
                    .chain(
                        self.m
                            .outbounds
                            .iter()
                            .filter(|o| o.connection == conn)
                            .map(|o| format!("outbound:{}#", o.event)),
                    )
                    .collect();
                let mut cur = Some(self.cause.clone());
                while let Some(c) = cur {
                    if names.iter().any(|n| c.desc.starts_with(n.as_str())) {
                        return Ok(V::Bool(true));
                    }
                    cur = c.parent.clone();
                }
                return Ok(V::Bool(false));
            }
            // `publish … on price, stock`: whether the record was created in this transaction
            // or one of these fields changed in it
            "__changed" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                let Some(id) = pos.first().and_then(|v| v.as_ref_id()) else {
                    return Ok(V::Bool(false));
                };
                let fields: Vec<String> = pos[1..].iter().map(|v| v.to_string()).collect();
                let tx = self.store.tx;
                let changed = self.store.records.get(&id).map_or(false, |r| {
                    r.created_tx == tx
                        || r.history
                            .iter()
                            .any(|(t, f, _)| *t == tx && fields.iter().any(|x| x == f.as_str()))
                });
                return Ok(V::Bool(changed));
            }
            "convert" => {
                let (pos, named) = self.eval_args(args, ctx)?;
                let x = pos.into_iter().next().unwrap_or(V::None);
                let get = |k: &str| named.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                let to = self.code_of(&get("to").unwrap_or(V::None))?;
                let from = match &x {
                    V::Money { cur, .. } | V::Price { cur, .. } => cur.to_string(),
                    V::None => return Ok(V::None),
                    other => return fail(format!("convert() needs an amount, found {other}")),
                };
                let rate = match get("rate").filter(|v| !v.is_none()) {
                    Some(r) => r.num().unwrap_or(Decimal::ONE),
                    None => {
                        let on = get("on").unwrap_or(V::Date(self.clock.date()));
                        self.exchange_rate(&from, &to, on)?
                    }
                };
                return Ok(match x {
                    V::Money { amt, .. } => V::Money {
                        amt: amt * rate,
                        cur: Rc::from(to.as_str()),
                    },
                    V::Price { amt, per, .. } => V::Price {
                        amt: amt * rate,
                        cur: Rc::from(to.as_str()),
                        per,
                    },
                    other => other,
                });
            }
            "why" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                let Some(V::Ref { id, ent }) = pos.first() else {
                    return Ok(text(""));
                };
                let Some(cause) = self.store.get(*id).map(|r| r.cause.clone()) else {
                    return Ok(text(""));
                };
                let mut lines = vec![format!(
                    "{ent} {} created",
                    self.display(&V::Ref {
                        ent: ent.clone(),
                        id: *id
                    })
                )];
                for c in cause.chain() {
                    lines.push(format!("  because {c}"));
                }
                return Ok(text(&lines.join("\n")));
            }
            "login" if self.viewer => return fail(
                "login() is not allowed here: an expression of an app runs as whoever looks at it",
            ),
            "login" => {
                let (pos, named) = self.eval_args(args, ctx)?;
                let login = pos.first().map(|v| v.to_string()).unwrap_or_default();
                self.user = self.login_id(&login);
                if let Some((_, c)) = named.iter().find(|(n, _)| n == "company") {
                    self.company = c.as_ref_id();
                }
                return Ok(V::None);
            }
            "Qty" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                let v = pos.first().and_then(|v| v.num()).unwrap_or_default();
                let unit = self.code_of(pos.get(1).unwrap_or(&V::None))?;
                return Ok(V::Qty {
                    v,
                    unit: Rc::from(unit.as_str()),
                });
            }
            "Price" => {
                let (pos, named) = self.eval_args(args, ctx)?;
                let (amt, cur) = match pos.first() {
                    Some(V::Money { amt, cur }) => (*amt, cur.clone()),
                    _ => (Decimal::ZERO, Rc::from("")),
                };
                let per = self.code_of(
                    &named
                        .iter()
                        .find(|(n, _)| n == "per")
                        .map(|(_, v)| v.clone())
                        .unwrap_or(V::None),
                )?;
                return Ok(V::Price {
                    amt,
                    cur,
                    per: Rc::from(per.as_str()),
                });
            }
            "DateRange" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                return match (pos.first(), pos.get(1)) {
                    (Some(V::Date(a)), Some(V::Date(b))) => Ok(V::DateRange(*a, *b)),
                    _ => fail("DateRange needs two dates"),
                };
            }
            "provisional" => {
                let (pos, _) = self.eval_args(args, ctx)?;
                return Ok(pos.into_iter().next().unwrap_or(V::None));
            }
            "affected" => {
                let Some(ExprKind::Name(ent)) = args.first().map(|a| &a.value.kind) else {
                    return fail("affected() needs an entity");
                };
                return Ok(V::Int(self.affected_count(ent) as i64));
            }
            "add" if self.post_buffer.is_some() => {
                let (_, named) = self.eval_args(args, ctx)?;
                self.post_buffer
                    .as_mut()
                    .unwrap()
                    .push(named.into_iter().collect());
                return Ok(V::None);
            }
            _ => {}
        }
        if let Some(f) = self.m.function(n) {
            let f = f.clone();
            let saved = self.system;
            self.system += 1;
            let r = self.call_fn(&f, args, ctx, None, None);
            self.system = saved;
            return r;
        }
        if let Some(metric) = self.m.metrics.iter().find(|x| x.name == n) {
            let (pos, named) = self.eval_args(args, ctx)?;
            let mut mctx = Ctx::default();
            let mut pos = pos.into_iter();
            for p in &metric.params {
                let v = named
                    .iter()
                    .find(|(x, _)| x == &p.name)
                    .map(|(_, v)| v.clone())
                    .or_else(|| pos.next())
                    .unwrap_or(V::None);
                mctx.bind(&p.name, v);
            }
            let Some((_, body)) = metric.formulas.first() else {
                return Ok(V::None);
            };
            return self.eval(body, &mut mctx);
        }
        if self.m.event(n).is_some() {
            let (_, named) = self.eval_args(args, ctx)?;
            return Ok(V::Event {
                name: Rc::from(n),
                fields: Rc::new(named),
            });
        }
        // a variant with data: `Discount(off=10%)`, `Fixed(8.00 EUR)`; fields in declared order
        if let Some(variant) = self.m.variant(n).filter(|v| !v.fields.is_empty()) {
            let names: Vec<String> = variant.fields.iter().map(|(f, _)| f.clone()).collect();
            let (pos, named) = self.eval_args(args, ctx)?;
            let mut pos = pos.into_iter();
            let fields: Vec<V> = names
                .iter()
                .map(|f| {
                    named
                        .iter()
                        .find(|(x, _)| x == f)
                        .map(|(_, v)| v.clone())
                        .or_else(|| pos.next())
                        .unwrap_or(V::None)
                })
                .collect();
            return Ok(V::Variant {
                name: Rc::from(n),
                fields: Rc::new(fields),
            });
        }
        if self.m.entity(n).is_some() {
            let outer = self.ctor_depth == 0;
            self.ctor_depth += 1;
            let r = self.eval_args(args, ctx);
            self.ctor_depth -= 1;
            let (_, named) = r?;
            let rec = self.create(n, named)?;
            if outer && self.system == 0 && self.user.is_some() {
                self.flush_defaults()?;
                self.check_permission("create", n, None, Some(&rec))?;
            }
            return Ok(rec);
        }
        // action or method of the current record called by its bare name
        if let (Some(this), Some(ent)) = (ctx.this.clone(), ctx.entity.clone()) {
            return self
                .method(this, n, args, ctx)
                .or_else(|_| fail(format!("unknown function `{n}` in {ent}")));
        }
        fail(format!("unknown function `{n}`"))
    }

    pub fn exchange_rate(&mut self, from: &str, to: &str, on: V) -> R<Decimal> {
        if from == to {
            return Ok(Decimal::ONE);
        }
        let f = self.m.function("exchange_rate").cloned();
        let (Some(f), Some(src), Some(dst)) = (
            f,
            self.by_code("Currency", from),
            self.by_code("Currency", to),
        ) else {
            return fail(format!("Missing exchange rate {from}/{to}"));
        };
        let saved = self.system;
        self.system += 1;
        let r = self.invoke_fn(&f, vec![src, dst, on], Vec::new(), None, None);
        self.system = saved;
        Ok(r?.num().unwrap_or(Decimal::ONE))
    }

    // ---- methods ----------------------------------------------------------------------

    fn lambda_arg(
        &mut self,
        args: &[Arg],
        ctx: &mut Ctx,
        idx: usize,
        name: Option<&str>,
    ) -> R<Option<V>> {
        let a = match name {
            Some(n) => args.iter().find(|a| a.name.as_deref() == Some(n)),
            None => args.iter().filter(|a| a.name.is_none()).nth(idx),
        };
        match a {
            Some(a) => Ok(Some(self.eval(&a.value, ctx)?)),
            None => Ok(None),
        }
    }

    fn method(&mut self, base: V, name: &str, args: &[Arg], ctx: &mut Ctx) -> R<V> {
        // an instant read in a zone: `dt.in("America/New_York").date`
        if let (V::DateTime(dt), "in") = (&base, name) {
            let (pos, _) = self.eval_args(args, ctx)?;
            let tz = self.zone_of(pos.first().unwrap_or(&V::None))?;
            return Ok(self.zoned(*dt, tz));
        }
        // collections and tables
        let items: Option<(Vec<V>, bool)> = match &base {
            V::List(v) => Some((v.to_vec(), false)),
            V::Set(v) => Some((v.to_vec(), true)),
            V::Table(t) => {
                let filter = self.pushdown(args, ctx);
                if self.m.entity(t).map_or(false, |e| e.is_ledger) {
                    self.post_written();
                    let rows = self
                        .store
                        .ledger_rows(t, filter.as_ref().map(|(c, v)| (c.as_str(), v)));
                    let rows: Vec<V> = rows
                        .iter()
                        .filter(|r| ctx.as_of.map_or(true, |a| r.tx <= a))
                        .map(|r| V::Ref {
                            ent: r.ledger.clone(),
                            id: r.id,
                        })
                        .collect();
                    Some((rows, false))
                } else {
                    let ids = match &filter {
                        Some((col, v)) => {
                            let names: Vec<String> = match self.m.entity(t) {
                                Some(e) if e.kind == EntityKind::Trait => self
                                    .m
                                    .implementors(t)
                                    .iter()
                                    .map(|e| e.name.clone())
                                    .collect(),
                                _ => vec![t.to_string()],
                            };
                            let mut ids = Vec::new();
                            let mut all = false;
                            for n in &names {
                                match self.store.find(n, col, v) {
                                    Some(found) => ids.extend(found),
                                    None => all = true,
                                }
                            }
                            if all {
                                self.ids(t, ctx.as_of)
                            } else {
                                ids.sort();
                                ids
                            }
                        }
                        None => self.ids(t, ctx.as_of),
                    };
                    let ids = if self.viewing() {
                        self.viewer_rows(t, ids)?
                    } else {
                        ids
                    };
                    let mut out = Vec::new();
                    for id in ids {
                        out.push(V::Ref {
                            ent: self.ent_of(id),
                            id,
                        });
                    }
                    Some((out, false))
                }
            }
            _ => None,
        };
        if let Some((items, set)) = items {
            if name == "all" && args.is_empty() && matches!(base, V::Table(_)) {
                return Ok(list(items));
            }
            return self.coll_method(items, set, name, args, ctx);
        }
        match &base {
            V::Ref { ent, id } => {
                let actual = self.ent_of(*id);
                let ent_name = if actual.is_empty() {
                    ent.to_string()
                } else {
                    actual.to_string()
                };
                let e = self.entity(&ent_name)?;
                if let Some(a) = e.actions.iter().find(|a| a.name == name) {
                    let a = a.clone();
                    let (pos, named) = self.eval_args(args, ctx)?;
                    return self.run_action(&base, &ent_name, name, Some(&a), pos, named);
                }
                if e.flows
                    .iter()
                    .any(|f| f.transitions.iter().any(|t| t.by.as_deref() == Some(name)))
                {
                    return self.run_action(&base, &ent_name, name, None, Vec::new(), Vec::new());
                }
                if let Some(f) = e.methods.iter().find(|f| f.name == name) {
                    let f = f.clone();
                    return self.call_fn(&f, args, ctx, Some(base.clone()), Some(ent_name));
                }
                match name {
                    "copy" => {
                        let (_, named) = self.eval_args(args, ctx)?;
                        self.copy(&ent_name, *id, named)
                    }
                    "closure" => {
                        let f = self.lambda_arg(args, ctx, 0, None)?.unwrap_or(V::None);
                        let mut out: Vec<V> = Vec::new();
                        let mut frontier = vec![base.clone()];
                        while let Some(x) = frontier.pop() {
                            let next = self.apply(&f, vec![x])?;
                            let nexts: Vec<V> = match next {
                                V::None => vec![],
                                V::List(v) | V::Set(v) => v.to_vec(),
                                other => vec![other],
                            };
                            for n in nexts {
                                if !n.is_none() && !out.contains(&n) {
                                    out.push(n.clone());
                                    if out.len() < 10_000 {
                                        frontier.push(n);
                                    }
                                }
                            }
                        }
                        Ok(V::Set(Rc::new(out)))
                    }
                    _ => fail(format!("`{ent_name}` has no action or method `{name}`")),
                }
            }
            _ => {
                let (pos, named) = self.eval_args(args, ctx)?;
                self.scalar_method(base, name, pos, named)
            }
        }
    }

    fn scalar_method(&mut self, base: V, name: &str, pos: Vec<V>, named: Vec<(String, V)>) -> R<V> {
        let arg = |i: usize| pos.get(i).cloned().unwrap_or(V::None);
        Ok(match (&base, name) {
            (V::Money { amt, cur }, "round") => match named
                .iter()
                .find(|(n, _)| n == "step")
                .and_then(|(_, v)| v.num())
            {
                Some(step) => V::Money {
                    amt: (amt / step).round_dp_with_strategy(0, self.rounding(cur)) * step,
                    cur: cur.clone(),
                },
                None => V::Money {
                    amt: self.round_money(*amt, cur),
                    cur: cur.clone(),
                },
            },
            (V::Money { amt, cur }, "abs") => V::Money {
                amt: amt.abs(),
                cur: cur.clone(),
            },
            (V::Price { amt, cur, per }, "round") => V::Price {
                amt: amt
                    .round_dp_with_strategy(self.decimals(cur) + 2, self.rounding(cur))
                    .normalize(),
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Price { amt, cur, per }, "to") => {
                let to = self.code_of(&arg(0))?;
                // price per `per` → price per `to`: multiply by the size of `to` in `per`
                let factor = self.convert_qty(Decimal::ONE, &to, per)?;
                V::Price {
                    amt: amt * factor,
                    cur: cur.clone(),
                    per: Rc::from(to.as_str()),
                }
            }
            (V::Price { amt, cur, per }, "abs") => V::Price {
                amt: amt.abs(),
                cur: cur.clone(),
                per: per.clone(),
            },
            (V::Qty { v, unit }, "to") => {
                let to = self.code_of(&arg(0))?;
                V::Qty {
                    v: self.convert_qty(*v, unit, &to)?,
                    unit: Rc::from(to.as_str()),
                }
            }
            (V::Qty { v, unit }, "round") | (V::Qty { v, unit }, "round_up") => {
                let precision = self
                    .units
                    .get(&**unit)
                    .map(|u| u.2)
                    .unwrap_or(parse_dec("0.001"));
                let steps = v / precision;
                let steps = if name == "round_up" {
                    steps.ceil()
                } else {
                    steps.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven)
                };
                V::Qty {
                    v: (steps * precision).normalize(),
                    unit: unit.clone(),
                }
            }
            (V::Qty { v, unit }, "abs") => V::Qty {
                v: v.abs(),
                unit: unit.clone(),
            },
            (V::Duration(s), "to") => {
                let to = self.code_of(&arg(0))?;
                let hours = d(*s) / d(3600);
                V::Qty {
                    v: self.convert_qty(hours, "h", &to)?.normalize(),
                    unit: Rc::from(to.as_str()),
                }
            }
            (V::Dec(x), "as_percent") | (V::Pct(x), "as_percent") => V::Pct(*x),
            (V::Int(i), "as_percent") => V::Pct(Decimal::from(*i)),
            (V::Int(_), "round" | "round_up") => base.clone(),
            (V::Dec(x), "round") => V::Dec(x.round_dp(2)),
            // a percentage to 2 decimals of the percentage: 12.345% → 12.34%
            (V::Pct(x), "round") => V::Pct(x.round_dp(4)),
            (V::Dec(x), "abs") => V::Dec(x.abs()),
            (V::Pct(x), "abs") => V::Pct(x.abs()),
            (V::Int(i), "abs") => V::Int(i.abs()),
            (V::Text(t), "len") => V::Int(t.chars().count() as i64),
            (V::Text(t), "starts_with") => V::Bool(t.starts_with(&arg(0).to_string())),
            (V::Text(t), "ends_with") => V::Bool(t.ends_with(&arg(0).to_string())),
            (V::Text(t), "contains") => V::Bool(t.contains(&arg(0).to_string())),
            (V::Text(t), "upper") => text(&t.to_uppercase()),
            (V::Text(t), "lower") => text(&t.to_lowercase()),
            (V::Text(t), "trim") => text(t.trim()),
            // the first n characters
            (V::Text(t), "take") => text(
                &t.chars()
                    .take(arg(0).num().and_then(|n| n.to_usize()).unwrap_or(0))
                    .collect::<String>(),
            ),
            // the first letter of each word: "Magazzino centrale" → "Mc"
            (V::Text(t), "initials") => text(
                &t.split_whitespace()
                    .filter_map(|w| w.chars().next())
                    .collect::<String>(),
            ),
            (V::Text(t), "sequence_part") => V::Int(
                t.rsplit('/')
                    .next()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0),
            ),
            (V::Date(dt), "end_of_month") => V::Date(end_of_month(*dt)),
            (V::Date(dt), "start_of_month") => V::Date(start_of_month(*dt)),
            // the first and the last second of the month
            (V::DateTime(dt), "start_of_month") => V::DateTime(time::instant(
                start_of_month(time::local(*dt, self.tz).date())
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
                self.tz,
            )),
            (V::DateTime(dt), "end_of_month") => V::DateTime(time::instant(
                end_of_month(time::local(*dt, self.tz).date())
                    .and_hms_opt(23, 59, 59)
                    .unwrap(),
                self.tz,
            )),
            (V::DateRange(a, b), "contains") => match arg(0) {
                V::Date(x) => V::Bool(*a <= x && x <= *b),
                _ => V::Bool(false),
            },
            _ => return fail(format!("{base} has no method `{name}`")),
        })
    }

    fn coll_method(
        &mut self,
        items: Vec<V>,
        set: bool,
        name: &str,
        args: &[Arg],
        ctx: &mut Ctx,
    ) -> R<V> {
        let f = self.lambda_arg(args, ctx, 0, None)?;
        let wrap = |v: Vec<V>| if set { V::Set(Rc::new(v)) } else { list(v) };
        let map = |s: &mut Self, x: &V| -> R<V> {
            match &f {
                Some(fun @ V::Lambda(_)) => s.apply(fun, vec![x.clone()]),
                _ => Ok(x.clone()),
            }
        };
        match name {
            "where" => {
                let mut out = Vec::new();
                for x in items {
                    if map(self, &x)?.truthy() {
                        out.push(x);
                    }
                }
                Ok(wrap(out))
            }
            "map" => {
                let mut out = Vec::new();
                for x in &items {
                    out.push(map(self, x)?);
                }
                Ok(list(out))
            }
            "flat_map" => {
                let mut out = Vec::new();
                for x in &items {
                    match map(self, x)? {
                        V::None => {}
                        V::List(v) | V::Set(v) => out.extend(v.iter().cloned()),
                        other => out.push(other),
                    }
                    // checked while it grows: the product of two long lists never gets built
                    if let Some(max) = self.max_list.filter(|m| out.len() > *m) {
                        let m = crate::limits::too_long_list(max);
                        crate::limits::hit(m.clone());
                        return fail(m);
                    }
                }
                Ok(list(out))
            }
            "sum" => {
                let mut acc = V::Int(0);
                for x in &items {
                    let v = map(self, x)?;
                    if v.is_none() {
                        continue;
                    }
                    acc = self.add_sub(true, acc, v)?;
                }
                // canonical decimals: the same whether the interpreter or the database added
                Ok(acc.normalized())
            }
            "count" => {
                if f.is_none() {
                    return Ok(V::Int(items.len() as i64));
                }
                let mut n = 0;
                for x in &items {
                    if map(self, x)?.truthy() {
                        n += 1;
                    }
                }
                Ok(V::Int(n))
            }
            "any" | "all" | "none" => {
                if f.is_none() {
                    return Ok(V::Bool(match name {
                        "any" => !items.is_empty(),
                        "none" => items.is_empty(),
                        _ => true,
                    }));
                }
                for x in &items {
                    let t = map(self, x)?.truthy();
                    match name {
                        "any" if t => return Ok(V::Bool(true)),
                        "all" if !t => return Ok(V::Bool(false)),
                        "none" if t => return Ok(V::Bool(false)),
                        _ => {}
                    }
                }
                Ok(V::Bool(name != "any"))
            }
            "first" | "last" if matches!(args, [Arg { name: None, value, .. }] if !matches!(value.kind, ExprKind::Lambda { .. })) =>
            {
                // `first(n)` / `last(n)`
                let n = match self.eval(&args[0].value, ctx)? {
                    V::Int(n) if n >= 0 => n as usize,
                    other => {
                        return fail(format!("{name}(n) takes a whole number ≥ 0, not {other}"))
                    }
                };
                let n = n.min(items.len());
                Ok(wrap(if name == "first" {
                    items[..n].to_vec()
                } else {
                    items[items.len() - n..].to_vec()
                }))
            }
            "avg" => {
                let mut acc = V::Int(0);
                let mut n = 0i64;
                for x in &items {
                    let v = map(self, x)?;
                    if v.is_none() {
                        continue;
                    }
                    acc = self.add_sub(true, acc, v)?;
                    n += 1;
                }
                if n == 0 {
                    return Ok(V::None);
                }
                let acc = match acc {
                    V::Int(i) => V::Dec(Decimal::from(i)),
                    other => other,
                };
                Ok(self.binop(BinOp::Div, acc, V::Int(n))?.normalized())
            }
            "first" | "last" => {
                if f.is_none() {
                    let x = if name == "first" {
                        items.first()
                    } else {
                        items.last()
                    };
                    return Ok(x.cloned().unwrap_or(V::None));
                }
                let mut found = None;
                for x in &items {
                    if map(self, x)?.truthy() {
                        found = Some(x.clone());
                        if name == "first" {
                            break;
                        }
                    }
                }
                Ok(found.unwrap_or(V::None))
            }
            "find" => {
                for x in &items {
                    if map(self, x)?.truthy() {
                        return Ok(x.clone());
                    }
                }
                Ok(V::None)
            }
            "single" => {
                let mut found = Vec::new();
                for x in &items {
                    if f.is_none() || map(self, x)?.truthy() {
                        found.push(x.clone());
                    }
                }
                match found.len() {
                    1 => Ok(found.pop().unwrap()),
                    0 => fail("single(): no element matches"),
                    n => fail(format!("single(): {n} elements match, expected one")),
                }
            }
            "contains" => {
                let x = self.eval(&args[0].value, ctx)?;
                Ok(V::Bool(items.iter().any(|i| *i == x)))
            }
            "distinct" => {
                let mut out: Vec<V> = Vec::new();
                for x in items {
                    if !out.contains(&x) {
                        out.push(x);
                    }
                }
                Ok(list(out))
            }
            "is_contiguous" => {
                let mut nums: Vec<i64> = items
                    .iter()
                    .filter_map(|v| if let V::Int(i) = v { Some(*i) } else { None })
                    .collect();
                nums.sort();
                Ok(V::Bool(nums.windows(2).all(|w| w[1] == w[0] + 1)))
            }
            "sort_by" | "last_by" | "max" | "min" => {
                let mut keyed = Vec::new();
                for x in &items {
                    let k = map(self, x)?;
                    // `max`/`min` are of the values there are
                    if k.is_none() && matches!(name, "max" | "min") {
                        continue;
                    }
                    keyed.push((k, x.clone()));
                }
                let mut err = None;
                keyed.sort_by(|a, b| match self.compare(&a.0, &b.0) {
                    Ok(c) => c.cmp(&0),
                    Err(e) => {
                        err = Some(e);
                        std::cmp::Ordering::Equal
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
                Ok(match name {
                    "sort_by" => list(keyed.into_iter().map(|(_, x)| x).collect()),
                    "last_by" => keyed.last().map(|(_, x)| x.clone()).unwrap_or(V::None),
                    "max" => keyed.last().map(|(k, _)| k.clone()).unwrap_or(V::None),
                    _ => keyed.first().map(|(k, _)| k.clone()).unwrap_or(V::None),
                })
            }
            "group_by" => {
                let f2 = self.lambda_arg(args, ctx, 1, None)?;
                let mut groups: Vec<((V, V), Vec<V>)> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for x in &items {
                    let k = map(self, x)?;
                    let k2 = match &f2 {
                        Some(fun @ V::Lambda(_)) => self.apply(fun, vec![x.clone()])?,
                        _ => V::None,
                    };
                    // equal values print alike: a hash of the printed keys finds the group,
                    // equality decides (linear only among keys printed alike)
                    let h = format!("{k:?}\u{1}{k2:?}");
                    match index
                        .get(&h)
                        .copied()
                        .filter(|&i| groups[i].0 .0 == k && groups[i].0 .1 == k2)
                    {
                        Some(i) => groups[i].1.push(x.clone()),
                        None => match groups.iter_mut().find(|((a, b), _)| *a == k && *b == k2) {
                            Some((_, v)) => v.push(x.clone()),
                            None => {
                                index.insert(h, groups.len());
                                groups.push(((k, k2), vec![x.clone()]));
                            }
                        },
                    }
                }
                let two = f2.is_some();
                Ok(list(
                    groups
                        .into_iter()
                        .map(|((k, k2), v)| group_tuple(k, two.then_some(k2), list(v)))
                        .collect(),
                ))
            }
            "allocate" => {
                let total = args
                    .first()
                    .filter(|a| a.name.is_none())
                    .map(|a| self.eval(&a.value, ctx))
                    .transpose()?
                    .unwrap_or(V::None);
                let by = self.lambda_arg(args, ctx, 0, Some("by"))?;
                let mut weights = Vec::new();
                for x in &items {
                    let w = match &by {
                        Some(fun @ V::Lambda(_)) => self.apply(fun, vec![x.clone()])?,
                        _ => V::Int(1),
                    };
                    weights.push(w.num().unwrap_or_default());
                }
                let shares = self.allocate(&total, &weights)?;
                Ok(list(
                    items
                        .into_iter()
                        .zip(shares)
                        .map(|(item, share)| {
                            V::Tuple(Rc::new(vec![
                                (Some("item".into()), item),
                                (Some("share".into()), share),
                            ]))
                        })
                        .collect(),
                ))
            }
            _ => fail(format!("collections have no method `{name}`")),
        }
    }

    /// Split `total` proportionally to `weights` so that the shares add up exactly:
    /// largest-remainder method in the smallest unit of the currency, ties in order.
    pub fn allocate(&self, total: &V, weights: &[Decimal]) -> R<Vec<V>> {
        let V::Money { amt, cur } = total else {
            return fail(format!("allocate() needs an amount, found {total}"));
        };
        if weights.is_empty() {
            return Ok(Vec::new());
        }
        let unit = Decimal::new(1, self.decimals(cur));
        let negative = amt.is_sign_negative();
        let units = (amt.abs() / unit).round();
        let wsum: Decimal = weights.iter().map(|w| w.abs()).sum();
        let ws: Vec<Decimal> = if wsum.is_zero() {
            vec![Decimal::ONE; weights.len()]
        } else {
            weights.iter().map(|w| w.abs()).collect()
        };
        let wsum: Decimal = ws.iter().copied().sum();
        let raw: Vec<Decimal> = ws.iter().map(|w| units * w / wsum).collect();
        let mut floors: Vec<Decimal> = raw.iter().map(|r| r.floor()).collect();
        let mut left = units - floors.iter().copied().sum::<Decimal>();
        let mut order: Vec<usize> = (0..raw.len()).collect();
        order.sort_by(|&a, &b| {
            (raw[b] - floors[b])
                .cmp(&(raw[a] - floors[a]))
                .then(a.cmp(&b))
        });
        for i in order {
            if left <= Decimal::ZERO {
                break;
            }
            floors[i] += Decimal::ONE;
            left -= Decimal::ONE;
        }
        Ok(floors
            .into_iter()
            .map(|f| {
                let v = f * unit;
                V::Money {
                    amt: if negative { -v } else { v },
                    cur: cur.clone(),
                }
            })
            .collect())
    }

    fn copy(&mut self, ent: &str, id: Id, overrides: Vec<(String, V)>) -> R<V> {
        let e = self.entity(ent)?;
        let mut args: Vec<(String, V)> = Vec::new();
        let flow_fields: HashSet<&str> = e.flows.iter().map(|f| f.field.as_str()).collect();
        let this = V::Ref {
            ent: Rc::from(ent),
            id,
        };
        let mut children: Vec<(String, Vec<V>)> = Vec::new();
        for f in self.m.all_fields(e) {
            if f.formula.is_some()
                || overrides.iter().any(|(n, _)| n == &f.name)
                || flow_fields.contains(f.name.as_str())
            {
                continue;
            }
            // a copy gets its own number and its own values for single-field unique fields
            let alone_unique = f.unique
                || self
                    .m
                    .all_uniques(e)
                    .iter()
                    .any(|u| u.fields.len() == 1 && u.fields[0] == f.name);
            if f.name == "number" || alone_unique {
                continue;
            }
            if let Some(Ty::List(inner)) = &f.ty {
                // composition children (whose back-reference is `parent`) are copied too;
                // other inverse relations (journal entries, payments…) are not
                let child = ty_names(Some(inner)).unwrap_or_default();
                let composition = self.m.entity(&child).map_or(false, |ce| {
                    ce.fields.iter().any(|cf| {
                        cf.parent
                            && (ty_names(cf.ty.as_ref()).as_deref() == Some(ent)
                                || e.traits
                                    .contains(&ty_names(cf.ty.as_ref()).unwrap_or_default()))
                    })
                });
                if composition {
                    let v = self.get(&this, &f.name, None, None)?;
                    let mut copies = Vec::new();
                    for c in v.items().unwrap_or(&[]).to_vec() {
                        if let V::Ref { ent: ce, id: cid } = &c {
                            copies.push(self.copy(ce, *cid, Vec::new())?);
                        }
                    }
                    children.push((f.name.clone(), copies));
                }
                if self
                    .m
                    .entity(&child)
                    .map_or(false, |ce| ce.kind != EntityKind::Record)
                {
                    continue;
                }
            }
            if self.field(ent, &f.name).map_or(false, |x| x.parent) {
                continue;
            }
            let v = self.get(&this, &f.name, None, None)?;
            if !v.is_none() {
                args.push((f.name.clone(), v));
            }
        }
        for (n, v) in children {
            if !overrides.iter().any(|(o, _)| *o == n) {
                args.push((n, list(v)));
            }
        }
        args.extend(overrides);
        self.create(ent, args)
    }

    // ---- statements -------------------------------------------------------------------

    pub fn stmts(&mut self, stmts: &[Stmt], ctx: &mut Ctx) -> R<()> {
        for s in stmts {
            self.stmt(s, ctx)?;
        }
        Ok(())
    }

    pub fn stmt(&mut self, s: &Stmt, ctx: &mut Ctx) -> R<()> {
        match &s.kind {
            StmtKind::Let { name, value, .. } => {
                let v = self.eval(value, ctx)?;
                ctx.bind(name, v);
            }
            StmtKind::If {
                binding,
                cond,
                then,
                otherwise,
            } => {
                let v = self.eval(cond, ctx)?;
                let go = if binding.is_some() {
                    !v.is_none()
                } else {
                    v.truthy()
                };
                if go {
                    ctx.scopes.push(Default::default());
                    if let Some(b) = binding {
                        ctx.bind(b, v);
                    }
                    let r = self.stmts(then, ctx);
                    ctx.scopes.pop();
                    r?;
                } else if let Some(o) = otherwise {
                    ctx.scopes.push(Default::default());
                    let r = self.stmts(o, ctx);
                    ctx.scopes.pop();
                    r?;
                }
            }
            StmtKind::For { names, iter, body } => {
                // `for x in Entity.where(…)`: page by page
                if self.query_loop(iter, names, body, ctx)?.is_some() {
                    return Ok(());
                }
                let it = self.eval(iter, ctx)?;
                for x in it.items().unwrap_or(&[]).to_vec() {
                    ctx.scopes.push(Default::default());
                    if names.len() == 1 {
                        ctx.bind(&names[0], x);
                    } else if let V::Tuple(t) = &x {
                        for (i, n) in names.iter().enumerate() {
                            ctx.bind(n, t.get(i).map(|(_, v)| v.clone()).unwrap_or(V::None));
                        }
                    }
                    let r = self.stmts(body, ctx);
                    ctx.scopes.pop();
                    r?;
                }
            }
            StmtKind::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, ctx)?,
                    None => V::None,
                };
                return Err(Flow::Return(v));
            }
            StmtKind::Require { cond, message } => {
                if !self.eval(cond, ctx)?.truthy() {
                    let msg = match message {
                        Some(m) => self.eval(m, ctx)?.to_string(),
                        None => format!("requirement failed: {}", print::expr(cond)),
                    };
                    return fail(msg);
                }
            }
            StmtKind::Fail(e) => {
                let v = self.eval(e, ctx)?;
                return fail(v.to_string());
            }
            StmtKind::Emit(e) => {
                let v = self.eval(e, ctx)?;
                self.store.outbox.push(v);
            }
            StmtKind::Receive { .. } | StmtKind::Deliver { .. } | StmtKind::ExpectSent { .. } => {
                return fail("`receive`, `deliver` and `expect sent` are statements of a test");
            }
            StmtKind::Assign { target, op, value } => {
                // `x.f = provisional(v, reason=…, until=…)`: the value plus a marker next to it
                let marker = match &value.kind {
                    ExprKind::Call { callee, args } if matches!(&callee.kind, ExprKind::Name(n) if n == "provisional") =>
                    {
                        let (_, named) = self.eval_args(args, ctx)?;
                        Some(V::Tuple(Rc::new(
                            named.into_iter().map(|(k, v)| (Some(k), v)).collect(),
                        )))
                    }
                    _ => None,
                };
                if let (Some(m), ExprKind::Field { base, name, .. }) = (marker, &target.kind) {
                    let rec = self.eval(base, ctx)?;
                    let v = self.eval(value, ctx)?;
                    self.set(&rec, name, v, ctx.company)?;
                    if let V::Ref { id, .. } = rec {
                        self.store.write(id, &format!("{name}#provisional"), m);
                    }
                    return Ok(());
                }
                let mut v = self.eval(value, ctx)?;
                let (rec, name) = match &target.kind {
                    ExprKind::Field { base, name, .. } => (self.eval(base, ctx)?, name.clone()),
                    ExprKind::Name(n) if ctx.local(n).is_some() => {
                        if *op != AssignOp::Set {
                            let old = ctx.local(n).cloned().unwrap_or(V::None);
                            v = self.add_sub(*op == AssignOp::Add, old, v)?;
                        }
                        ctx.assign(n, v);
                        return Ok(());
                    }
                    ExprKind::Name(n) => (ctx.this.clone().unwrap_or(V::None), n.clone()),
                    _ => return fail("cannot assign to this expression"),
                };
                if *op != AssignOp::Set {
                    let old = self.get(&rec, &name, None, None)?;
                    v = self.add_sub(*op == AssignOp::Add, old, v)?;
                }
                self.set(&rec, &name, v, ctx.company)?;
            }
            StmtKind::Expr(e) => {
                self.eval(e, ctx)?;
            }
            StmtKind::Fn(f) => {
                let captured = ctx
                    .scopes
                    .iter()
                    .flat_map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())))
                    .collect();
                self.local_fns.insert(f.name.clone(), (f.clone(), captured));
            }
            StmtKind::Meta { .. } | StmtKind::Migration { .. } | StmtKind::Block { .. } => {}
            StmtKind::As { user, body } => {
                let saved = self.user;
                self.user = self.login_id(user);
                let r = self.stmts(body, ctx);
                self.user = saved;
                r?;
            }
            // test-only statements are handled by the runner
            StmtKind::Expect(_)
            | StmtKind::ExpectError { .. }
            | StmtKind::ExpectCompileError { .. }
            | StmtKind::Use(_)
            | StmtKind::Clock(_) => {}
        }
        Ok(())
    }

    // ---- actions, flows, reactions ----------------------------------------------------

    pub fn run_action(
        &mut self,
        rec: &V,
        ent: &str,
        name: &str,
        action: Option<&Action>,
        pos: Vec<V>,
        named: Vec<(String, V)>,
    ) -> R<V> {
        if self.viewing() {
            return fail(format!(
                "{ent}.{name}: an action cannot run in a formula of an app (it only reads)"
            ));
        }
        self.check_permission("run", ent, Some(name), Some(rec))?;
        self.enter(&format!("action `{ent}.{name}`"))?;
        let r = self.run_action_inner(rec, ent, name, action, pos, named);
        self.depth -= 1;
        r
    }

    fn run_action_inner(
        &mut self,
        rec: &V,
        ent: &str,
        name: &str,
        action: Option<&Action>,
        pos: Vec<V>,
        named: Vec<(String, V)>,
    ) -> R<V> {
        let e = self.entity(ent)?;
        let label = format!("{ent}.{name}");
        self.system += 1;
        let r = self.with_cause(label, |s| -> R<V> {
            // `before Entity.action` reactions
            for (i, r) in s.m.reactions.iter().enumerate() {
                if r.entity == ent && matches!(&r.trigger, Trigger::Before(a) if a == name) {
                    s.run_reaction(i, rec)?;
                }
            }
            // flow transition driven by this action
            let mut target = None;
            for flow in &e.flows {
                let state = s.get(rec, &flow.field, None, None)?;
                let mut has_by = false;
                for t in flow.transitions.iter().filter(|t| t.by.as_deref() == Some(name)) {
                    has_by = true;
                    let from_ok = matches!(&state, V::Variant { name: st, .. } if t.from.iter().any(|f| **f == **st));
                    if !from_ok {
                        continue;
                    }
                    let mut ctx = Ctx::for_record(rec, ent);
                    if let Some(w) = &t.when {
                        if !s.eval(w, &mut ctx)?.truthy() {
                            continue;
                        }
                    }
                    if let Some(u) = &t.unless {
                        if s.eval(u, &mut ctx)?.truthy() {
                            return fail(format!("{ent}.{name} is not allowed: {}", print::expr(u)));
                        }
                    }
                    target = Some((flow.field.clone(), t.to.clone()));
                    break;
                }
                if has_by && target.is_none() {
                    return fail(format!("{ent}.{name} is not allowed in state {state}"));
                }
            }
            // a transition that takes the record out of its frozen states: unfrozen now, so
            // the action's body may write what the transition needs
            if let (Some((field, to)), V::Ref { id, .. }) = (&target, rec) {
                if s.store.records.get(id).map_or(false, |r| r.frozen_at.is_some()) {
                    let state = V::Variant { name: Rc::from(to.as_str()), fields: Rc::new(Vec::new()) };
                    if !s.frozen_holds(*id, Some((field, state)))? {
                        s.thaw(*id);
                    }
                }
            }
            let mut result = V::None;
            if let Some(a) = action {
                let mut ctx = Ctx::for_record(rec, ent);
                let mut pos = pos.into_iter();
                for p in &a.params {
                    let v = match named.iter().find(|(n, _)| n == &p.name) {
                        Some((_, v)) => v.clone(),
                        None => match pos.next() {
                            Some(v) => v,
                            None => match &p.default {
                                Some(dx) => s.eval(dx, &mut ctx.clone())?,
                                None => V::None,
                            },
                        },
                    };
                    ctx.bind(&p.name, v);
                }
                match s.stmts(&a.body, &mut ctx) {
                    Ok(()) => {}
                    Err(Flow::Return(v)) => result = v,
                    Err(e) => return Err(e),
                }
            }
            if let Some((field, to)) = target {
                s.transition(rec, ent, &field, &to)?;
            }
            Ok(result)
        });
        self.system -= 1;
        r
    }

    fn transition(&mut self, rec: &V, ent: &str, field: &str, to: &str) -> R<()> {
        let V::Ref { id, .. } = rec else {
            return Ok(());
        };
        self.store.write(
            *id,
            field,
            V::Variant {
                name: Rc::from(to),
                fields: Rc::new(Vec::new()),
            },
        );
        if let Some(r) = self.store.records.get_mut(id) {
            r.pending_defaults.retain(|p| p != field);
        }
        if self
            .store
            .records
            .get(id)
            .map_or(false, |r| r.frozen_at.is_some())
            && !self.frozen_holds(*id, None)?
        {
            self.thaw(*id);
        }
        let reactions: Vec<usize> = self
            .m
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.entity == ent && matches!(&r.trigger, Trigger::Enters(s) if s == to))
            .map(|(i, _)| i)
            .collect();
        for i in reactions {
            self.run_reaction(i, rec)?;
        }
        Ok(())
    }

    fn run_reaction(&mut self, i: usize, rec: &V) -> R<()> {
        let r = &self.m.reactions[i];
        let desc = match &r.trigger {
            Trigger::Enters(s) => format!("on {} enters {s}", r.entity),
            Trigger::Becomes(e) => format!("on {} becomes {}", r.entity, print::expr(e)),
            Trigger::Before(a) => format!("before {}.{a}", r.entity),
            Trigger::Created => format!("on {} created", r.entity),
            Trigger::Changes => format!("on {} changes", r.entity),
            Trigger::Delivered => format!("on {} delivered", r.entity),
            Trigger::Failed => format!("on {} failed", r.entity),
            Trigger::Expired => format!("on {} expired", r.entity),
            Trigger::Skipped => format!("on {} skipped", r.entity),
            Trigger::Rejected => format!("on {} rejected", r.entity),
        };
        self.system += 1;
        let res = self.with_cause(desc, |s| {
            let mut ctx = Ctx::default();
            ctx.bind(&r.binding, rec.clone());
            s.stmts(&r.body, &mut ctx)
        });
        self.system -= 1;
        match res {
            Err(Flow::Return(_)) => Ok(()),
            other => other,
        }
    }

    /// The reactions to the outcome of an event's delivery (`on OrderShipped delivered as d:`,
    /// `delivered` true; `on … failed as f:`, false), each given `outcome`, run as the system in
    /// the current transaction. Returns how many ran.
    pub fn run_outcome(&mut self, event: &str, delivered: bool, outcome: &V) -> R<usize> {
        let want = if delivered {
            Trigger::Delivered
        } else {
            Trigger::Failed
        };
        self.run_outcome_of(event, &want, outcome)
    }

    /// The reactions `on <name> <want> as x:` (an outcome: delivered, failed, expired, skipped,
    /// rejected), each with `outcome`. Returns how many ran.
    pub fn run_outcome_of(&mut self, event: &str, want: &Trigger, outcome: &V) -> R<usize> {
        let want = want.clone();
        let found: Vec<usize> = self
            .m
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.entity == event && r.trigger == want)
            .map(|(i, _)| i)
            .collect();
        for &i in &found {
            self.run_reaction(i, outcome)?;
        }
        Ok(found.len())
    }

    /// Automatic transitions, `becomes` reactions and ledger postings, until stable.
    pub fn settle(&mut self) -> R<()> {
        for _ in 0..20 {
            self.flush_defaults()?;
            self.store_in_field_units()?;
            let mut changed = false;
            // automatic transitions (`when` without `by`)
            for e in &self.m.entities {
                for (fi, flow) in e.flows.iter().enumerate() {
                    for (ti, t) in flow
                        .transitions
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.by.is_none() && t.when.is_some())
                    {
                        let deps = self.transition_deps(e, fi, ti);
                        for id in self.candidates(&e.name, &deps) {
                            if *self.ent_of(id) != *e.name {
                                continue;
                            }
                            let rec = V::Ref {
                                ent: Rc::from(e.name.as_str()),
                                id,
                            };
                            let state = self.get(&rec, &flow.field, None, None)?;
                            if !matches!(&state, V::Variant { name, .. } if t.from.iter().any(|f| **f == **name))
                            {
                                continue;
                            }
                            let mut ctx = Ctx::for_record(&rec, &e.name);
                            if self.eval(t.when.as_ref().unwrap(), &mut ctx)?.truthy() {
                                self.system += 1;
                                let r = self.with_cause(
                                    format!("{}.{} → {} (automatic)", e.name, flow.field, t.to),
                                    |s| s.transition(&rec, &e.name, &flow.field, &t.to),
                                );
                                self.system -= 1;
                                r?;
                                changed = true;
                            }
                        }
                    }
                }
            }
            // `becomes` reactions: fire when the condition turns true
            for (i, r) in self.m.reactions.iter().enumerate() {
                let Trigger::Becomes(cond) = &r.trigger else {
                    continue;
                };
                let deps = self.deps(&format!("becomes:{i}"), &r.entity, &[cond], &[]);
                for id in self.candidates(&r.entity, &deps) {
                    let rec = V::Ref {
                        ent: Rc::from(r.entity.as_str()),
                        id,
                    };
                    let mut ctx = Ctx::for_record(&rec, &r.entity);
                    let now = self.eval(cond, &mut ctx)?.truthy();
                    let before = match self.store.becomes.insert((i, id), now) {
                        Some(b) => b,
                        // first time seen in this working set: the value before this transaction
                        None if self.store.lazy()
                            && self
                                .store
                                .records
                                .get(&id)
                                .map_or(false, |r| r.created_tx < self.store.tx) =>
                        {
                            let mut c2 = Ctx::for_record(&rec, &r.entity);
                            c2.as_of = Some(self.store.tx - 1);
                            // a sweep: the condition was last evaluated when the record was last
                            // written or at the previous sweep, whichever came later, and with the
                            // clock of then (`becomes at <= now()` turns true between two sweeps)
                            let saved = self.clock;
                            if self.sweep {
                                let version = self.store.records.get(&id).map(|r| r.version());
                                let written =
                                    version.and_then(|t| self.store.commit_info(t)).map(|c| c.0);
                                if let Some(then) = written.into_iter().chain(self.last_sweep).max()
                                {
                                    self.clock = then.min(saved);
                                }
                            }
                            let before = self
                                .eval(cond, &mut c2)
                                .map(|v| v.truthy())
                                .unwrap_or(false);
                            self.clock = saved;
                            before
                        }
                        None => false,
                    };
                    if now && !before && !self.store.fired.contains(&(i, id)) {
                        self.store.fired.insert((i, id));
                        // caused by whatever was changed last
                        let last = if self.store.lazy() {
                            let dirty: Vec<Id> = self.store.dirty.iter().copied().collect();
                            dirty
                                .iter()
                                .filter_map(|d| self.store.records.get(d))
                                .max_by_key(|r| (r.created_tx, r.id))
                                .map(|r| r.cause.clone())
                        } else {
                            self.store
                                .records
                                .values()
                                .max_by_key(|r| (r.created_tx, r.id))
                                .map(|r| r.cause.clone())
                        };
                        let saved = self.cause.clone();
                        if let Some(c) = last {
                            self.cause = c;
                        }
                        let res = self.run_reaction(i, &rec);
                        self.cause = saved;
                        res?;
                        changed = true;
                    }
                }
            }
            self.flush_defaults()?;
            if self.post_ledgers()? {
                changed = true;
            }
            // `created` / `changes` reactions, once the rest is stable
            if !changed && self.record_reactions()? {
                changed = true;
            }
            if !changed {
                return Ok(());
            }
        }
        fail("the automations do not converge (more than 20 rounds)")
    }

    /// `on X created` for each record created in this transaction, `on X changes` for each
    /// existing record written in it: once per record and transaction. True if any ran.
    fn record_reactions(&mut self) -> R<bool> {
        let reactions: Vec<usize> = self
            .m
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.trigger, Trigger::Created | Trigger::Changes))
            .map(|(i, _)| i)
            .collect();
        if reactions.is_empty() {
            return Ok(false);
        }
        let tx = self.store.tx;
        let written: Vec<Id> = self.store.dirty.iter().copied().collect();
        let mut ran = false;
        for i in reactions {
            let r = &self.m.reactions[i];
            for &id in &written {
                let Some(rec) = self.store.records.get(&id) else {
                    continue;
                };
                let ent = rec.ent.clone();
                let created = rec.created_tx == tx;
                let fire = match r.trigger {
                    Trigger::Created => created,
                    _ => !created && rec.history.iter().any(|(t, _, _)| *t == tx),
                };
                if !fire || self.store.fired.contains(&(i, id)) {
                    continue;
                }
                if *ent != *r.entity
                    && !self
                        .m
                        .entity(&ent)
                        .map_or(false, |e| self.m.implements(e, &r.entity))
                {
                    continue;
                }
                self.store.fired.insert((i, id));
                self.run_reaction(i, &V::Ref { ent, id })?;
                ran = true;
            }
        }
        Ok(ran)
    }

    /// Recompute what each record posts; post the difference (reversal + new rows).
    fn post_ledgers(&mut self) -> R<bool> {
        let mut changed = false;
        for e in &self.m.entities {
            for pi in 0..e.posts.len() {
                let deps = self.post_deps(e, pi);
                for id in self.candidates(&e.name, &deps) {
                    if *self.ent_of(id) == *e.name && self.post_record(e, pi, id)? {
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Post now what the records written so far in this transaction post, so that a read of a
    /// ledger later in the same transaction sees their rows (a `before` that needs the stock
    /// received earlier in a fixture). The commit recomputes every posting anyway; a record
    /// that cannot post yet (it is still being built) is left to it.
    fn post_written(&mut self) {
        if self.posting_early || self.store.writes == self.posted_at_writes {
            return;
        }
        self.posting_early = true;
        let written: Vec<Id> = self.store.dirty.iter().copied().collect();
        for id in written {
            let ent = self.ent_of(id);
            let Some(e) = self.m.entity(&ent) else {
                continue;
            };
            for pi in 0..e.posts.len() {
                let _ = self.post_record(e, pi, id);
            }
        }
        self.posted_at_writes = self.store.writes;
        self.posting_early = false;
    }

    /// Post the difference between what record `id` posts now with its `pi`-th `post` and what
    /// it posted before. True if rows were posted.
    fn post_record(&mut self, e: &'m Entity, pi: usize, id: Id) -> R<bool> {
        let post = &e.posts[pi];
        let rec = V::Ref {
            ent: Rc::from(e.name.as_str()),
            id,
        };
        let mut ctx = Ctx::for_record(&rec, &e.name);
        let active = match &post.when {
            Some(w) => self.eval(w, &mut ctx)?.truthy(),
            None => true,
        };
        let mut desired: Vec<HashMap<String, V>> = Vec::new();
        if active {
            let outer = self.post_buffer.replace(Vec::new());
            let r = self.stmts(&post.body, &mut ctx);
            desired = std::mem::replace(&mut self.post_buffer, outer).unwrap_or_default();
            r?;
        }
        let key = (id, format!("{}#{pi}", post.ledger));
        let current = self.store.postings_of(key.0, &key.1);
        if current == desired {
            return Ok(false);
        }
        let date = self.clock;
        let tx = self.store.tx;
        let ledger: Rc<str> = Rc::from(post.ledger.as_str());
        let measures: Vec<String> = self
            .m
            .entity(&post.ledger)
            .map(|l| {
                l.fields
                    .iter()
                    .filter(|f| f.kind == FieldKind::LedgerMeasure)
                    .map(|f| f.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // rows posted earlier in this transaction are taken back, not reversed
        let earlier = self.store.posted_rows.remove(&key);
        if let Some(ids) = &earlier {
            self.store.unpost(ids);
        }
        for row in current.iter().filter(|_| earlier.is_none()) {
            let mut rev = row.clone();
            for m in &measures {
                if let Some(v) = rev.get(m).cloned() {
                    rev.insert(m.clone(), neg(v)?);
                }
            }
            let rid = self.store.new_id();
            self.store.push_row(LedgerRow {
                ledger: ledger.clone(),
                id: rid,
                cols: rev,
                source: Some(rec.clone()),
                date,
                tx,
            });
        }
        let mut ids = Vec::new();
        for row in &desired {
            let rid = self.store.new_id();
            self.store.posted.push(rid);
            ids.push(rid);
            self.store.push_row(LedgerRow {
                ledger: ledger.clone(),
                id: rid,
                cols: row.clone(),
                source: Some(rec.clone()),
                date,
                tx,
            });
        }
        self.store.posted_rows.insert(key.clone(), ids);
        self.store.postings.insert(key, desired);
        Ok(true)
    }

    // ---- constraints at commit --------------------------------------------------------

    /// Every constraint of every record. Warnings are attached to the record.
    pub fn check_all(&mut self) -> R<()> {
        self.check_provisional()?;
        // required values missing, reported together and in creation order: the record the
        // user wrote comes before the ones its defaults created from the missing values
        let mut missing: Vec<(Id, String)> = Vec::new();
        for e in &self.m.entities {
            if e.kind == EntityKind::Trait {
                continue;
            }
            if e.is_ledger {
                self.check_ledger(e)?;
                continue;
            }
            let deps = self.record_checks_deps(e);
            let mut ids = self.candidates(&e.name, &deps);
            ids.extend(self.thawed_of(&e.name));
            ids.sort();
            ids.dedup();
            // a record reached only through what some of its `check`s read: those checks
            // (the others read nothing written and hold as before)
            let narrow = self.narrow_checks(e, &ids);
            // the records and what their rules read through references, in a few queries
            if ids.len() > 1 {
                let (full, part): (Vec<Id>, Vec<Id>) =
                    ids.iter().partition(|id| !narrow.contains_key(id));
                self.store.prefetch_refs(&full, 3);
                self.store.prefetch_refs(&part, 2);
            }
            for id in ids {
                if *self.ent_of(id) != *e.name {
                    continue;
                }
                match narrow.get(&id) {
                    Some(sel) => self.check_record_sel(e, id, Some(sel), &mut missing)?,
                    None => self.check_record(e, id, &mut missing)?,
                }
            }
            if self.store.lazy() {
                self.check_uniques_written(e)?;
                continue;
            }
            for u in self.m.all_uniques(e) {
                let mut seen: Vec<Vec<V>> = Vec::new();
                for id in self.ids(&e.name, None) {
                    let rec = V::Ref {
                        ent: self.ent_of(id),
                        id,
                    };
                    let mut ctx = Ctx::for_record(&rec, &e.name);
                    if let Some(w) = &u.when {
                        if !self.eval(w, &mut ctx)?.truthy() {
                            continue;
                        }
                    }
                    let mut key = Vec::new();
                    for f in &u.fields {
                        key.push(self.get(&rec, f, None, None)?);
                    }
                    if key.iter().any(|k| k.is_none()) {
                        continue;
                    }
                    if seen.contains(&key) {
                        let shown: Vec<String> = key.iter().map(|k| self.display(k)).collect();
                        return fail(unique_message(&e.name, &u.fields, u.when.is_some(), &shown));
                    }
                    seen.push(key);
                }
            }
            for f in self.m.all_fields(e).into_iter().filter(|f| f.unique) {
                let mut seen: Vec<V> = Vec::new();
                for id in self.ids(&e.name, None) {
                    let v = self.get(
                        &V::Ref {
                            ent: Rc::from(e.name.as_str()),
                            id,
                        },
                        &f.name,
                        None,
                        None,
                    )?;
                    if v.is_none() {
                        continue;
                    }
                    if seen.contains(&v) {
                        let shown = [self.display(&v)];
                        return fail(unique_message(
                            &e.name,
                            std::slice::from_ref(&f.name),
                            false,
                            &shown,
                        ));
                    }
                    seen.push(v);
                }
            }
        }
        if self.whole_data && !self.violations.is_empty() {
            return fail(self.violations_report());
        }
        if !missing.is_empty() {
            missing.sort_by_key(|(id, _)| *id);
            let mut msgs: Vec<String> = Vec::new();
            for (_, m) in missing {
                if !msgs.contains(&m) {
                    msgs.push(m);
                }
            }
            return fail(msgs.into_iter().take(6).collect::<Vec<_>>().join("; "));
        }
        Ok(())
    }

    /// The rules of one record, as at the end of a transaction: required values (missing
    /// ones are added to `missing`, reported together by the caller), the `where` of its
    /// fields, the rules of the `record` values it holds, `check` and `warn`. Warnings are
    /// attached to the record. Also used by the bulk import, record by record.
    pub fn check_record(
        &mut self,
        e: &'m Entity,
        id: Id,
        missing: &mut Vec<(Id, String)>,
    ) -> R<()> {
        self.check_record_sel(e, id, None, missing)
    }

    /// `check_record` limited to the rules `sel` selects (every rule when None): a deploy
    /// checks on the existing records only the rules it adds or changes.
    pub fn check_record_sel(
        &mut self,
        e: &'m Entity,
        id: Id,
        sel: Option<&RuleSel>,
        missing: &mut Vec<(Id, String)>,
    ) -> R<()> {
        let all = sel.map_or(true, |s| s.all);
        let warnings = all || sel.map_or(false, |s| s.warnings);
        let rec = V::Ref {
            ent: Rc::from(e.name.as_str()),
            id,
        };
        if warnings {
            if let Some(r) = self.store.records.get_mut(&id) {
                r.warnings.clear();
            }
        }
        // the rules of a frozen record held when it was frozen: the `where` of its
        // fields is not checked again against later data (only on a field written now);
        // whether it is frozen is looked up when a `where` is checked
        let mut frozen: Option<bool> = None;
        let tx = self.store.tx;
        let missing_before = missing.len();
        for f in self.m.all_fields(e) {
            if f.formula.is_some() {
                continue;
            }
            let want_req = all || sel.map_or(false, |s| s.required.contains(&f.name));
            let want_where =
                (all || sel.map_or(false, |s| s.wheres.contains(&f.name))) && f.where_.is_some();
            if !want_req && !want_where {
                continue;
            }
            // a collection computed from the other side is never required: nothing to check
            // without a `where` (and nothing to load)
            if !want_where && f.required_when.is_none() && inverse_field(self.m, e, f) {
                continue;
            }
            let mut ctx = Ctx::for_record(&rec, &e.name);
            let v = self.get(&rec, &f.name, None, None)?;
            let optional = matches!(
                f.ty,
                Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_)) | None
            );
            if v.is_none() {
                let required = match &f.required_when {
                    Some(c) if want_req => self.eval(c, &mut ctx)?.truthy(),
                    Some(_) => false,
                    None => !optional && want_req,
                };
                if required && f.kind == FieldKind::Stored && !f.per_company {
                    missing.push((id, format!("{}.{} is required", e.name, f.name)));
                }
                continue;
            }
            let written_now = || {
                self.store.records.get(&id).map_or(false, |r| {
                    r.history.iter().any(|(t, n, _)| *t == tx && n == &f.name)
                })
            };
            let recheck = want_where
                && (written_now()
                    || !*frozen.get_or_insert_with(|| self.frozen_owner(id).is_some()));
            if let (Some(w), true) = (&f.where_, recheck) {
                let values: Vec<V> = match &v {
                    V::List(items) | V::Set(items) => items.to_vec(),
                    other => vec![other.clone()],
                };
                for v in values {
                    if !self.satisfies_where(w, &v, &mut ctx)? {
                        return fail(format!(
                            "{}.{} = {}: does not satisfy its `where`",
                            e.name,
                            f.name,
                            self.display(&v)
                        ));
                    }
                }
            }
            if all {
                self.check_unit_kind(e, f, &rec, &v)?;
            }
        }
        // the rules of the `record` values it holds
        if missing.len() == missing_before {
            for f in self.m.all_fields(e) {
                if !(warnings || sel.map_or(false, |s| s.records.contains(&f.name))) {
                    continue;
                }
                if f.formula.is_none()
                    && self
                        .m
                        .entity(&ty_names_inner(f.ty.as_ref()).unwrap_or_default())
                        .map_or(false, |x| x.kind == EntityKind::Record)
                {
                    let v = self.get(&rec, &f.name, None, None)?;
                    self.check_record_values(id, &v)?;
                }
            }
        }
        // an incomplete record: its checks would only report consequences
        if missing.len() > missing_before {
            return Ok(());
        }
        for (i, c) in self.m.all_checks(e).into_iter().enumerate() {
            if !(all || (c.warn && warnings) || sel.map_or(false, |s| s.checks.contains(&i))) {
                continue;
            }
            let mut ctx = Ctx::for_record(&rec, &e.name);
            let held = self.eval_rule(e, id, &rec, &c.cond, &mut ctx)?;
            if !held.truthy() {
                let msg = self
                    .eval(&c.message, &mut ctx)
                    .map(|m| m.to_string())
                    .unwrap_or_else(|_| print::expr(&c.cond));
                if c.warn {
                    if let Some(r) = self.store.records.get_mut(&id) {
                        r.warnings.push(msg);
                    }
                } else if self.whole_data {
                    self.violation(e, c, id, &rec, msg);
                } else {
                    return fail(msg);
                }
            }
        }
        Ok(())
    }

    /// A rule of a record, evaluated while committing: a rollup it reads that this transaction
    /// changed by delta makes it a guard (`rollup OP value`, checked again by whoever orders
    /// the commits, on the latest value) when it has that shape, and a read otherwise.
    fn eval_rule(&mut self, e: &'m Entity, id: Id, rec: &V, cond: &Expr, ctx: &mut Ctx) -> R<V> {
        let outer = self.capture.replace(Vec::new());
        let was = std::mem::replace(&mut self.delta_free, true);
        let v = self.eval(cond, ctx);
        let captured = std::mem::replace(&mut self.capture, outer).unwrap_or_default();
        let v = match v {
            Ok(v) => v,
            Err(err) => {
                self.delta_free = was;
                return Err(err);
            }
        };
        if !captured.is_empty() {
            let guard = if v.truthy() {
                self.guard_of(e, id, rec, cond, &captured, ctx)
            } else {
                None
            };
            match guard {
                Some(g) => self.store.guards.push(g),
                None => {
                    if !self.store.untracked {
                        for (cid, f) in &captured {
                            self.store.reads.formula(*cid, f);
                        }
                    }
                }
            }
            if let Some(c) = &mut self.capture {
                c.extend(captured);
            }
        }
        self.delta_free = was;
        Ok(v)
    }

    /// `cond` as a guard: a comparison of one of the record's own rollups changed by delta
    /// (the only one it read) with a value that reads none.
    fn guard_of(
        &mut self,
        e: &'m Entity,
        id: Id,
        rec: &V,
        cond: &Expr,
        captured: &[(Id, String)],
        ctx: &mut Ctx,
    ) -> Option<crate::store::Guard> {
        let ExprKind::Binary { op, lhs, rhs } = &cond.kind else {
            return None;
        };
        let op = *op;
        if !matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            return None;
        }
        let own = |x: &Expr| match &x.kind {
            ExprKind::Name(n) => Some(n.clone()),
            ExprKind::Field { base, name, .. } if matches!(base.kind, ExprKind::SelfRef) => {
                Some(name.clone())
            }
            _ => None,
        };
        let field = captured.first().map(|(_, f)| f.clone())?;
        if captured.iter().any(|(i, f)| *i != id || *f != field) {
            return None;
        }
        let (bound, op) = if own(lhs).as_deref() == Some(field.as_str()) {
            (rhs, op)
        } else if own(rhs).as_deref() == Some(field.as_str()) {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Le => BinOp::Ge,
                BinOp::Gt => BinOp::Lt,
                _ => BinOp::Le,
            };
            (lhs, flipped)
        } else {
            return None;
        };
        if !self
            .m
            .all_fields(e)
            .iter()
            .any(|f| f.name == field && f.formula.is_some())
        {
            return None;
        }
        // the other side, on its own: it may read no rollup changed by delta
        let outer = self.capture.replace(Vec::new());
        let value = self.eval(bound, ctx);
        let read = std::mem::replace(&mut self.capture, outer).unwrap_or_default();
        let value = value.ok()?;
        if !read.is_empty() {
            return None;
        }
        let _ = rec;
        Some(crate::store::Guard {
            id,
            field,
            op,
            bound: value,
        })
    }

    /// A deploy changed `frozen when`: freeze the record if it holds now, unfreeze it if it
    /// no longer does. Whether the record changed.
    pub fn refreeze(&mut self, id: Id) -> R<bool> {
        let ent = self.ent_of(id);
        let Some(e) = self.m.entity(&ent) else {
            return Ok(false);
        };
        let Some(fr) = &e.frozen else {
            return Ok(false);
        };
        match self.store.records.get(&id).map(|r| r.frozen_at) {
            Some(Some(_)) => {
                if !self.frozen_holds(id, None)? {
                    self.thaw(id);
                    return Ok(true);
                }
            }
            Some(None) => {
                let rec = V::Ref {
                    ent: ent.clone(),
                    id,
                };
                if self
                    .eval(&fr.when, &mut Ctx::for_record(&rec, &ent))?
                    .truthy()
                {
                    let tx = self.store.tx;
                    if let Some(r) = self.store.records.get_mut(&id) {
                        r.frozen_at = Some(tx);
                    }
                    self.store.epoch += 1;
                    self.store.dirty.insert(id);
                    return Ok(true);
                }
            }
            None => {}
        }
        Ok(false)
    }

    /// The uniqueness rules of an entity: (fields, `when`).
    pub fn unique_rules(&self, e: &Entity) -> Vec<(Vec<String>, Option<Expr>)> {
        let mut keys: Vec<(Vec<String>, Option<Expr>)> = self
            .m
            .all_uniques(e)
            .into_iter()
            .map(|u| (u.fields.clone(), u.when.clone()))
            .collect();
        keys.extend(
            self.m
                .all_fields(e)
                .into_iter()
                .filter(|f| f.unique)
                .map(|f| (vec![f.name.clone()], None)),
        );
        keys
    }

    /// A record that breaks a `check`, while every record is checked against a new model.
    fn violation(&mut self, e: &Entity, c: &quadra_ir::Check, id: Id, rec: &V, message: String) {
        let at = match self
            .violations
            .iter()
            .position(|v| v.span == c.span && v.entity == e.name)
        {
            Some(i) => i,
            None => {
                self.violations.push(Violation {
                    entity: e.name.clone(),
                    span: c.span,
                    message,
                    records: Vec::new(),
                    count: 0,
                });
                self.violations.len() - 1
            }
        };
        self.violations[at].count += 1;
        if self.violations[at].records.len() >= VIOLATIONS_SHOWN {
            return;
        }
        // the fields the rule reads, with the values that break it
        let cond = print::expr(&c.cond);
        let word = |name: &str| {
            cond.match_indices(name).any(|(i, _)| {
                let before = cond[..i].chars().last();
                let after = cond[i + name.len()..].chars().next();
                let part = |c: Option<char>| c.map_or(false, |c| c.is_alphanumeric() || c == '_');
                !part(before) && !part(after) && before != Some('.')
            })
        };
        let names: Vec<String> = self
            .m
            .all_fields(e)
            .into_iter()
            .filter(|f| word(&f.name))
            .map(|f| f.name.clone())
            .collect();
        let mut values = Vec::new();
        for n in names {
            if let Ok(v) = self.get(rec, &n, None, None) {
                let shown = if v.is_none() {
                    "none".to_string()
                } else {
                    self.display(&v)
                };
                values.push((n, shown));
            }
        }
        let who = self.display(rec);
        let who = if who.contains('#') {
            who
        } else {
            format!("{} {who} (#{id})", e.name)
        };
        self.violations[at].records.push((id, who, values));
    }

    /// The broken rules with the code that declares them, the records that break them, and
    /// what can be done.
    pub fn violations_report(&self) -> String {
        let mut out = String::new();
        let n = self.violations.len();
        out.push_str(if n == 1 {
            "a rule of the new code is broken by data you already have:\n"
        } else {
            "rules of the new code are broken by data you already have:\n"
        });
        for v in &self.violations {
            out.push('\n');
            match self.m.place(v.span) {
                Some((path, line, text)) => out.push_str(&format!("  {path}:{line}\n    {text}\n")),
                None => out.push_str(&format!("  {}: {}\n", v.entity, v.message)),
            }
            let what = if v.count == 1 {
                format!("1 {} breaks it", v.entity)
            } else {
                format!("{} records of {} break it", v.count, v.entity)
            };
            out.push_str(&format!("  {what}:\n"));
            for (_, who, values) in &v.records {
                let vals = values
                    .iter()
                    .map(|(f, x)| format!("{f} = {x}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!(
                    "    {who}{}\n",
                    if vals.is_empty() {
                        String::new()
                    } else {
                        format!(": {vals}")
                    }
                ));
            }
            if v.count > v.records.len() {
                out.push_str(&format!("    … and {} more\n", v.count - v.records.len()));
            }
        }
        out.push_str(&format!(
            "\n{} cannot hold on this data. Either:\n  - correct those records (in the app, or with a `change` for many of them);\n  - add a migration that fixes them: it runs before the rules are checked;\n  - or change the rule.",
            if n == 1 { "The rule" } else { "These rules" }
        ));
        out
    }

    /// An amount, quantity or price written in this transaction against the currency and
    /// unit its field declares, when they are only known at run time (`Qty<uom>`): another
    /// currency, or a unit of another kind, is refused (as the API does).
    fn check_unit_kind(&mut self, e: &Entity, f: &Field, rec: &V, v: &V) -> R<()> {
        if f.per_company || !matches!(v, V::Money { .. } | V::Qty { .. } | V::Price { .. }) {
            return Ok(());
        }
        let V::Ref { id, .. } = rec else {
            return Ok(());
        };
        let tx = self.store.tx;
        let written = self.store.records.get(id).map_or(false, |r| {
            r.history
                .iter()
                .any(|(t, name, _)| *t == tx && name == &f.name)
        });
        if !written {
            return Ok(());
        }
        let Some(z) = self.m.zeros.get(&f.span).cloned() else {
            return Ok(());
        };
        let Some(expected) = self.typed_zero(&z, Some(rec), None) else {
            return Ok(());
        };
        // units of different kinds: both known, different categories
        let other_kind = |s: &Self, a: &str, b: &str| {
            a != b
                && matches!((s.units.get(a), s.units.get(b)), (Some((_, c1, _)), Some((_, c2, _))) if c1 != c2)
        };
        let shown = self.display(v);
        match (v, &expected) {
            (V::Money { cur, .. }, V::Money { cur: want, .. }) if cur != want => fail(format!(
                "{}.{} = {shown}: expected an amount in {want}",
                e.name, f.name
            )),
            (V::Qty { unit, .. }, V::Qty { unit: want, .. }) if other_kind(self, unit, want) => {
                fail(format!(
                    "{}.{} = {shown}: expected a quantity in {want}",
                    e.name, f.name
                ))
            }
            (V::Price { cur, .. }, V::Price { cur: want, .. }) if cur != want => fail(format!(
                "{}.{} = {shown}: expected a price in {want}",
                e.name, f.name
            )),
            (V::Price { per, .. }, V::Price { per: want, .. }) if other_kind(self, per, want) => {
                fail(format!(
                    "{}.{} = {shown}: expected a price per {want}",
                    e.name, f.name
                ))
            }
            _ => Ok(()),
        }
    }

    /// Does `v` satisfy the `where` of its field? (`ctx` is the record holding it)
    fn satisfies_where(&mut self, w: &Where, v: &V, ctx: &mut Ctx) -> R<bool> {
        Ok(match w {
            Where::Compare(op, x) => {
                let rhs = self.eval(x, ctx)?;
                self.binop(*op, v.clone(), rhs)?.truthy()
            }
            Where::Expr(x) => match &x.kind {
                ExprKind::Lambda { params, body } => {
                    let mut c2 = ctx.clone();
                    c2.bind(&params[0], v.clone());
                    self.eval(body, &mut c2)?.truthy()
                }
                ExprKind::Binary {
                    op: BinOp::Range,
                    lhs,
                    rhs,
                } => {
                    let lo = self.eval(lhs, ctx)?;
                    let hi = self.eval(rhs, ctx)?;
                    self.compare(v, &lo)? >= 0 && self.compare(v, &hi)? <= 0
                }
                _ => self.eval(x, ctx)?.truthy(),
            },
        })
    }

    /// The rules written in a `record` (`where`, `required when`, `check`/`warn`) on the
    /// values a record `holder` stores (a single value or a list), nested records included.
    /// Warnings are attached to the holder.
    fn check_record_values(&mut self, holder: Id, v: &V) -> R<()> {
        let values: Vec<V> = match v {
            V::List(items) | V::Set(items) => items.to_vec(),
            other => vec![other.clone()],
        };
        for rv in values {
            let V::Rec { ent, .. } = &rv else { continue };
            let Some(re) = self.m.entity(ent) else {
                continue;
            };
            let mut ctx = Ctx::for_record(&rv, &re.name);
            for f in self.m.all_fields(re) {
                if f.formula.is_some() {
                    continue;
                }
                let fv = self.get(&rv, &f.name, None, None)?;
                if fv.is_none() {
                    let optional = matches!(
                        f.ty,
                        Some(Ty::Optional(_)) | Some(Ty::List(_)) | Some(Ty::Set(_)) | None
                    );
                    let required = match &f.required_when {
                        Some(c) => self.eval(c, &mut ctx)?.truthy(),
                        None => !optional,
                    };
                    if required {
                        return fail(format!("{}.{} is required", re.name, f.name));
                    }
                    continue;
                }
                if let Some(w) = &f.where_ {
                    let items: Vec<V> = match &fv {
                        V::List(items) | V::Set(items) => items.to_vec(),
                        other => vec![other.clone()],
                    };
                    for x in items {
                        if !self.satisfies_where(w, &x, &mut ctx)? {
                            return fail(format!(
                                "{}.{} = {}: does not satisfy its `where`",
                                re.name,
                                f.name,
                                self.display(&x)
                            ));
                        }
                    }
                }
                self.check_record_values(holder, &fv)?;
            }
            for c in self.m.all_checks(re) {
                if !self.eval(&c.cond, &mut ctx)?.truthy() {
                    let msg = self
                        .eval(&c.message, &mut ctx)
                        .map(|m| m.to_string())
                        .unwrap_or_else(|_| print::expr(&c.cond));
                    if c.warn {
                        if let Some(r) = self.store.records.get_mut(&holder) {
                            r.warnings.push(msg);
                        }
                    } else {
                        return fail(msg);
                    }
                }
            }
        }
        Ok(())
    }

    /// Uniqueness of the records written in this transaction, by index lookups.
    pub fn check_uniques_written(&mut self, e: &Entity) -> R<()> {
        let tx = self.store.tx;
        let written: Vec<Id> = self
            .store
            .dirty
            .iter()
            .copied()
            .filter(|id| {
                self.store.records.get(id).map_or(false, |r| {
                    *r.ent == *e.name && r.history.iter().any(|(t, _, _)| *t == tx)
                })
            })
            .collect();
        let mut keys: Vec<(Vec<String>, Option<Expr>)> = self
            .m
            .all_uniques(e)
            .into_iter()
            .map(|u| (u.fields.clone(), u.when.clone()))
            .collect();
        keys.extend(
            self.m
                .all_fields(e)
                .into_iter()
                .filter(|f| f.unique)
                .map(|f| (vec![f.name.clone()], None)),
        );
        for (fields, when) in keys {
            for &id in &written {
                // a key without `when` that this transaction did not write cannot have become
                // a duplicate: a record that now collides with it was written, and is checked
                let touched = self.store.records.get(&id).map_or(true, |r| {
                    r.created_tx == tx
                        || r.history.iter().any(|(t, f, _)| {
                            *t == tx
                                && fields
                                    .iter()
                                    .any(|k| f == k || f.starts_with(&format!("{k}@")))
                        })
                });
                if when.is_none() && !touched {
                    continue;
                }
                let key_of = |s: &mut Self, rid: Id| -> R<Option<Vec<V>>> {
                    let rec = V::Ref {
                        ent: Rc::from(e.name.as_str()),
                        id: rid,
                    };
                    if let Some(w) = &when {
                        if !s.eval(w, &mut Ctx::for_record(&rec, &e.name))?.truthy() {
                            return Ok(None);
                        }
                    }
                    let mut key = Vec::new();
                    for f in &fields {
                        key.push(s.get(&rec, f, None, None)?);
                    }
                    Ok(if key.iter().any(|k| k.is_none()) {
                        None
                    } else {
                        Some(key)
                    })
                };
                let Some(key) = key_of(self, id)? else {
                    continue;
                };
                let others = match self.store.find(&e.name, &fields[0], &key[0]) {
                    Some(ids) => ids,
                    None => self.ids(&e.name, None),
                };
                for o in others.into_iter().filter(|o| *o != id) {
                    if key_of(self, o)?.as_ref() == Some(&key) {
                        let shown: Vec<String> = key.iter().map(|k| self.display(k)).collect();
                        return fail(unique_message(&e.name, &fields, when.is_some(), &shown));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_ledger(&mut self, l: &Entity) -> R<()> {
        // plain checks: on every row posted in this transaction
        let plain: Vec<&Check> = l.checks.iter().filter(|c| c.per.is_empty()).collect();
        if !plain.is_empty() {
            let posted: Vec<Id> = self.store.posted.clone();
            for rid in posted {
                if !self
                    .store
                    .rows
                    .iter()
                    .any(|r| r.id == rid && *r.ledger == *l.name)
                {
                    continue;
                }
                let row = V::Ref {
                    ent: Rc::from(l.name.as_str()),
                    id: rid,
                };
                for c in &plain {
                    let mut ctx = Ctx::for_record(&row, &l.name);
                    if !self.eval(&c.cond, &mut ctx)?.truthy() {
                        let msg = self
                            .eval(&c.message, &mut ctx)
                            .map(|m| m.to_string())
                            .unwrap_or_else(|_| print::expr(&c.cond));
                        return fail(msg);
                    }
                }
            }
        }
        for c in l.checks.iter().filter(|c| !c.per.is_empty()) {
            let mut groups: Vec<(Vec<V>, Vec<V>)> = Vec::new();
            let rows: Vec<LedgerRow> = if self.store.lazy() {
                // only the groups that received rows in this transaction
                let tx = self.store.tx;
                let first = c.per[0].clone();
                let new: Vec<LedgerRow> = self
                    .store
                    .rows
                    .iter()
                    .filter(|r| *r.ledger == *l.name && r.tx == tx)
                    .cloned()
                    .collect();
                let mut out: Vec<LedgerRow> = Vec::new();
                let mut seen = HashSet::new();
                for r in &new {
                    let loaded = match r.cols.get(&first) {
                        Some(v) if first != "source" => {
                            self.store.ledger_rows(&l.name, Some((first.as_str(), v)))
                        }
                        _ => self.store.ledger_rows(&l.name, None),
                    };
                    for x in loaded {
                        if seen.insert(x.id) {
                            out.push(x);
                        }
                    }
                }
                out.sort_by_key(|r| r.id);
                out
            } else {
                self.store
                    .rows
                    .iter()
                    .filter(|r| *r.ledger == *l.name)
                    .cloned()
                    .collect()
            };
            for r in &rows {
                let key: Vec<V> = c
                    .per
                    .iter()
                    .map(|k| match k.as_str() {
                        "source" => r.source.clone().unwrap_or(V::None),
                        other => r.cols.get(other).cloned().unwrap_or(V::None),
                    })
                    .collect();
                let rv = V::Ref {
                    ent: r.ledger.clone(),
                    id: r.id,
                };
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(rv),
                    None => groups.push((key, vec![rv])),
                }
            }
            for (key, items) in groups {
                let key_tuple = V::Tuple(Rc::new(
                    c.per
                        .iter()
                        .cloned()
                        .map(Some)
                        .zip(key.into_iter())
                        .collect(),
                ));
                let ExprKind::Lambda { params, body } = &c.cond.kind else {
                    continue;
                };
                let mut ctx = Ctx::default();
                ctx.bind(&params[0], list(items));
                ctx.bind("__key", key_tuple.clone());
                let ok = self.eval_group(body, &mut ctx, &params[0], key_tuple)?;
                if !ok {
                    let msg = self
                        .eval(&c.message, &mut Ctx::default())
                        .map(|m| m.to_string())
                        .unwrap_or_default();
                    return fail(msg);
                }
            }
        }
        Ok(())
    }

    /// Evaluate a per-group check, where `g.key` is the group's key.
    fn eval_group(&mut self, body: &Expr, ctx: &mut Ctx, param: &str, key: V) -> R<bool> {
        let rewritten = rewrite_key(body, param);
        ctx.bind("__group_key", key);
        Ok(self.eval(&rewritten, ctx)?.truthy())
    }

    /// Unfreeze the frozen records whose `frozen when` no longer holds (before the checks:
    /// their rules apply again). Transitions do it as they happen; this covers the rest.
    pub fn thaw_records(&mut self) -> R<()> {
        for e in &self.m.entities {
            if e.frozen.is_none() {
                continue;
            }
            let deps = self.frozen_deps(e);
            for id in self.candidates(&e.name, &deps) {
                if *self.ent_of(id) != *e.name
                    || self
                        .store
                        .records
                        .get(&id)
                        .map_or(true, |r| r.frozen_at.is_none())
                {
                    continue;
                }
                if !self.frozen_holds(id, None)? {
                    self.thaw(id);
                }
            }
        }
        Ok(())
    }

    /// Freeze records whose `frozen when` became true (end of transaction).
    pub fn freeze(&mut self) -> R<()> {
        for e in &self.m.entities {
            let Some(fr) = &e.frozen else { continue };
            let deps = self.frozen_deps(e);
            for id in self.candidates(&e.name, &deps) {
                if *self.ent_of(id) != *e.name
                    || self
                        .store
                        .records
                        .get(&id)
                        .map_or(true, |r| r.frozen_at.is_some())
                {
                    continue;
                }
                let rec = V::Ref {
                    ent: Rc::from(e.name.as_str()),
                    id,
                };
                let mut ctx = Ctx::for_record(&rec, &e.name);
                if self.eval(&fr.when, &mut ctx)?.truthy() {
                    let tx = self.store.tx;
                    if let Some(r) = self.store.records.get_mut(&id) {
                        r.frozen_at = Some(tx);
                    }
                    self.store.epoch += 1;
                }
            }
        }
        Ok(())
    }

    /// Run `f` as one transaction: on error the store is restored.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        let snapshot = self.store.clone();
        self.store.tx += 1;
        self.store
            .commits
            .insert(self.store.tx, (self.clock, self.user));
        self.store.fired.clear();
        self.store.dirty.clear();
        self.store.wlog.clear();
        self.store.wlog_gen += 1;
        self.reset_incremental();
        self.store.posted.clear();
        self.store.posted_rows.clear();
        self.store.thawed.clear();
        self.store.deltas.clear();
        self.store.guards.clear();
        // the events of this transaction only: the commit writes them to the outbox
        self.store.outbox.clear();
        let r = f(self).and_then(|v| {
            self.settle()?;
            self.thaw_records()?;
            self.check_all()?;
            self.freeze()?;
            if let Some(e) = self.store.source_error() {
                return fail(format!("database: {e}"));
            }
            // a periodic sweep that found nothing to do writes no (empty) commit
            if self.sweep
                && self.store.dirty.is_empty()
                && self.store.rows.len() == snapshot.rows.len()
                && self.store.outbox.is_empty()
            {
                let (next, block) = (self.store.next_id, self.store.id_block());
                self.store = snapshot.clone();
                self.store.next_id = next;
                self.store.set_id_block(block);
                return Ok(v);
            }
            if self.sink.is_some() {
                self.materialize_formulas();
                let mut sink = self.sink.take().unwrap();
                let commit = Commit {
                    tx: self.store.tx,
                    base: self.store.base,
                    before: &snapshot,
                    after: &self.store,
                    user: self.user,
                    cause: self.cause.chain().join(" ← "),
                    time: self.clock,
                };
                let res = sink.commit(&commit);
                self.sink = Some(sink);
                match res {
                    Ok(assigned) => {
                        self.after_commit();
                        let local = self.store.tx;
                        self.store.renumber(local, assigned);
                        self.store.idempotency = None;
                        // nobody else committed in between: the working set is current
                        if assigned == self.store.base + 1 {
                            self.store.base = assigned;
                            self.store.reads = Default::default();
                        }
                    }
                    Err(CommitError::Conflict(c)) => return Err(Flow::Conflict(c)),
                    Err(CommitError::Failed(e)) => return fail(format!("database: {e}")),
                }
            }
            Ok(v)
        });
        if r.is_ok() {
            if let Some(sent) = &mut self.sent {
                sent.extend(self.store.outbox.iter().map(|v| (v.clone(), true)));
            }
        }
        if r.is_err() {
            self.reset_incremental();
            // ids are never reused, even after a rollback (as with a database sequence)
            let (next, block) = (self.store.next_id, self.store.id_block());
            self.store = snapshot;
            self.store.next_id = next;
            self.store.set_id_block(block);
        }
        r
    }

    /// Recompute the materialized formulas; changed values are written by the sink.
    pub fn materialize_formulas(&mut self) {
        let was = std::mem::replace(&mut self.delta_free, true);
        self.materialize_formulas_inner();
        self.delta_free = was;
    }

    fn materialize_formulas_inner(&mut self) {
        let targets = self.materialize.clone();
        for (ent, field) in targets {
            let ids = if self.store.lazy() && self.store.uid != 0 && self.trusted(&ent, &field) {
                // the records whose stored value may be stale (the others hold)
                self.stale_of(&ent, &field)
            } else if self.store.lazy() {
                let deps = self.formula_deps(&ent, &field);
                let mut ids = self.candidates(&ent, &deps);
                ids.extend(self.thawed_of(&ent));
                ids.sort();
                ids.dedup();
                ids
            } else {
                self.store.known_ids(&ent)
            };
            for id in ids {
                let rec = V::Ref {
                    ent: Rc::from(ent.as_str()),
                    id,
                };
                self.materializing = Some((id, field.clone()));
                let v = self.get(&rec, &field, None, None).unwrap_or(V::None);
                self.materializing = None;
                let key = (id, field.clone());
                if self.store.materialized.get(&key) != Some(&v) {
                    self.store.materialized.insert(key, v);
                }
            }
        }
    }
}

/// `g.key.x` → `__group_key.x` inside a per-group check.
fn rewrite_key(e: &Expr, param: &str) -> Expr {
    let mut out = e.clone();
    fn walk(e: &mut Expr, param: &str) {
        match &mut e.kind {
            ExprKind::Field { base, name, .. } => {
                if name == "key" && matches!(&base.kind, ExprKind::Name(n) if n == param) {
                    e.kind = ExprKind::Name("__group_key".into());
                } else {
                    walk(base, param);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                walk(lhs, param);
                walk(rhs, param);
            }
            ExprKind::Unary { expr, .. } => walk(expr, param),
            ExprKind::Call { callee, args } => {
                walk(callee, param);
                for a in args {
                    walk(&mut a.value, param);
                }
            }
            _ => {}
        }
    }
    walk(&mut out, param);
    out
}

fn is_bare_zero(v: &V) -> bool {
    match v {
        V::Int(0) => true,
        V::Dec(d) => d.is_zero(),
        _ => false,
    }
}

/// A collection computed from the other side (children, the inverse of a reference or a set).
fn inverse_field(m: &Model, owner: &Entity, f: &Field) -> bool {
    let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty else {
        return false;
    };
    let Some(child) = ty_names(Some(inner)).and_then(|c| m.entity(&c)) else {
        return false;
    };
    if child.kind == EntityKind::Record {
        return false;
    }
    f.through.is_some()
        || f.via.is_some()
        || child.fields.iter().any(|x| {
            let n = match &x.ty {
                Some(Ty::Optional(i)) => ty_names(Some(i)),
                other => ty_names(other.as_ref()),
            };
            n.is_some_and(|n| n == owner.name || owner.traits.contains(&n))
        })
}

fn ty_names(t: Option<&Ty>) -> Option<String> {
    match t? {
        Ty::Path { segments, .. } if segments.len() == 1 => Some(segments[0].clone()),
        Ty::Optional(i) => ty_names(Some(i)),
        _ => None,
    }
}

/// The type a field names, through `?` and lists: `Address`, `[Address]`, `Address?`.
pub(super) fn ty_names_inner_pub(t: Option<&Ty>) -> Option<String> {
    ty_names_inner(t)
}

fn ty_names_inner(t: Option<&Ty>) -> Option<String> {
    match t? {
        Ty::List(i) | Ty::Set(i) => ty_names_inner(Some(i)),
        other => ty_names(Some(other)),
    }
}

/// Conversions through factors like 1/60 leave noise in the last digits: 20 decimals are
/// far beyond any currency or unit precision.
fn tidy(x: Decimal) -> Decimal {
    x.round_dp(15).normalize()
}

/// The message of a broken `unique`: the same for a `unique` field and a one-field
/// `unique (n)`, in memory and on a database.
pub fn unique_message(ent: &str, fields: &[String], conditional: bool, shown: &[String]) -> String {
    if fields.len() == 1 && !conditional {
        format!(
            "{ent}.{} must be unique: {} already exists",
            fields[0], shown[0]
        )
    } else {
        format!(
            "unique ({}): {} already exists in {ent}",
            fields.join(", "),
            shown.join(", ")
        )
    }
}

/// A sequence pattern filled in: `{code}`, `{year}` and `{n}`, each with an optional format
/// (`FT/{year}/{n:05}` → `FT/2026/00042`).
fn sequence_number(pattern: &str, code: &str, year: i32, n: u64) -> String {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let inner = &rest[open + 1..open + close];
        let (name, spec) = match inner.split_once(':') {
            Some((name, spec)) => (name, FormatSpec::parse(spec)),
            None => (inner, None),
        };
        let (value, numeric) = match name {
            "code" => (code.to_string(), false),
            "year" => (year.to_string(), true),
            "n" => (n.to_string(), true),
            _ => (format!("{{{inner}}}"), false),
        };
        out.push_str(&match spec {
            Some(spec) => spec.pad(&value, numeric),
            None => value,
        });
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn start_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap()
}

fn end_of_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap() - Duration::days(1)
}

pub fn lit(l: &Lit) -> V {
    match l {
        Lit::Number(n) => {
            if n.contains('.') {
                V::Dec(parse_dec(n))
            } else {
                n.parse()
                    .map(V::Int)
                    .unwrap_or_else(|_| V::Dec(parse_dec(n)))
            }
        }
        Lit::Percent(p) => V::Pct(parse_dec(p) / hundred()),
        Lit::Str(s) => text(s),
        Lit::Date(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(V::Date)
            .unwrap_or(V::None),
        Lit::DateTime(s) => NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
            .map(V::DateTime)
            .unwrap_or(V::None),
        Lit::YearMonth(s) => {
            let (y, m) = s.split_once('-').unwrap_or(("0", "0"));
            V::YearMonth(y.parse().unwrap_or(0), m.parse().unwrap_or(0))
        }
        Lit::Bool(b) => V::Bool(*b),
        Lit::None => V::None,
    }
}

fn neg(v: V) -> R<V> {
    Ok(match v {
        V::Int(i) => V::Int(-i),
        V::Dec(x) => V::Dec(-x),
        V::Pct(x) => V::Pct(-x),
        V::Money { amt, cur } => V::Money { amt: -amt, cur },
        V::Price { amt, cur, per } => V::Price {
            amt: -amt,
            cur,
            per,
        },
        V::Qty { v, unit } => V::Qty { v: -v, unit },
        V::Duration(s) => V::Duration(-s),
        V::None => V::None,
        other => return fail(format!("cannot negate {other}")),
    })
}

/// A group of `group_by`: `(key, items)`, or `(key, key2, items)` with two keys.
pub(crate) fn group_tuple(key: V, key2: Option<V>, items: V) -> V {
    let mut f = vec![(Some("key".to_string()), key)];
    if let Some(k2) = key2 {
        f.push((Some("key2".to_string()), k2));
    }
    f.push((Some("items".to_string()), items));
    V::Tuple(Rc::new(f))
}
