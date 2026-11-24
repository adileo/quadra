//! Spreadsheets of apps: their formulas are Quadra expressions, evaluated on the server, as
//! the user who looks at them (`POST /v1/eval`; the HTTP API reference (quadra.run/reference/http-api), "Apps").
//!
//! A formula (`=…`) is an expression of the language with cell references: `A1`, `$A$1`,
//! ranges `A1:B10`, whole columns `A:A`, other sheets `Sales!B2`, `'My sheet'!A1:C3`, and the
//! array a formula spilled `A1#`. The references are replaced by names before the expression
//! is parsed (`rewrite`), so the rest is the language as it is: the same checker, the same
//! interpreter, the same SQL translation.
//!
//! - **Checking**: the workbook becomes one function of the project (`fn __sheet(…)`) checked
//!   with the code, like the expressions of a view: the cells holding values are its typed
//!   parameters (`5.40 EUR` is a `Money<EUR>`), each formula a `let`, in the order of their
//!   dependencies; a formula's type errors are the checker's messages (`cannot add Money<USD>
//!   to Money<EUR>`). The function depends only on the formulas and on the types of the
//!   values: editing a number does not check the workbook again (`checks` cache).
//! - **Ranges** are lists of their cells that hold something, row by row: `B2:B20.sum()`. A
//!   small range of known cells is a list of them (typed); a large one, a whole column or one
//!   reaching into a spill is a parameter filled when evaluating.
//! - **Spill**: a formula whose value is a list fills the cells below it (a list of tuples or
//!   of lists: below and to the right), as dynamic arrays do; `#SPILL` when a cell there holds
//!   something; at most `SPILL_ROWS` rows shown (the value itself stays whole for `A1#`).
//! - **Evaluation** as the viewer (`Interp::viewer`): queries keep the rows they may read,
//!   hidden fields fail with `#HIDDEN`, nothing is written. A formula reading the database is
//!   kept (`cache`) until a commit changes one of the entities it reads.
//! - **Errors** per cell: `#TYPE` (the checker), `#CYCLE`, `#REF`, `#SPILL`, `#HIDDEN`,
//!   `#NOACCESS`, `#TIMEOUT`, `#ERROR` (any other failure); a cell using a cell in error has
//!   that error too.

use super::*;
use quadra_ir::{Expr, ExprKind, StmtKind};
use quadra_runtime::Ctx;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

/// Rows of a spill shown in cells (the value is whole for `A1#`).
pub const SPILL_ROWS: usize = 10_000;
const SPILL_COLS: usize = 200;
/// The time a request may take (the user's rule: never a minute).
const DEADLINE_SECS: u64 = 20;
const MAX_SHEETS: usize = 50;
const MAX_CELLS: usize = 400_000;
const MAX_FORMULAS: usize = 20_000;
const MAX_FORMULA: usize = 4000;
pub const MAX_ROWS: u32 = 1_000_000;
pub const MAX_COLS: u32 = 16_384;
/// A range of at most this many known cells is a typed list; larger, a parameter.
const TYPED_RANGE: usize = 1000;

/// A cell: sheet (by position; the expressions of a markdown text are a sheet after the
/// others), row and column from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Addr {
    pub s: usize,
    pub r: u32,
    pub c: u32,
}

/// What a formula refers to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ref {
    Cell(Addr),
    /// a rectangle (`A1:B10`); whole columns have `r2 = MAX_ROWS`
    Range {
        s: usize,
        r1: u32,
        c1: u32,
        r2: u32,
        c2: u32,
    },
    /// the array the formula of this cell spilled
    Spill(Addr),
}

impl Ref {
    fn name(&self) -> String {
        match self {
            Ref::Cell(a) => format!("__c_{}_{}_{}", a.s, a.r, a.c),
            Ref::Range { s, r1, c1, r2, c2 } => format!("__r_{s}_{r1}_{c1}_{r2}_{c2}"),
            Ref::Spill(a) => format!("__sp_{}_{}_{}", a.s, a.r, a.c),
        }
    }
}

/// `A` → 1, `AB` → 28.
pub fn col_index(letters: &str) -> Option<u32> {
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let mut n = 0u32;
    for ch in letters.chars() {
        if !ch.is_ascii_uppercase() {
            return None;
        }
        n = n * 26 + (ch as u32 - 'A' as u32 + 1);
    }
    (n <= MAX_COLS).then_some(n)
}

/// 1 → `A`, 28 → `AB`.
pub fn col_name(mut c: u32) -> String {
    let mut out = Vec::new();
    while c > 0 {
        let k = (c - 1) % 26;
        out.push((b'A' + k as u8) as char);
        c = (c - 1) / 26;
    }
    out.iter().rev().collect()
}

/// `B12` → (12, 2).
pub fn parse_a1(a: &str) -> Option<(u32, u32)> {
    let split = a.find(|ch: char| ch.is_ascii_digit())?;
    let (l, d) = a.split_at(split);
    let c = col_index(l)?;
    if d.starts_with('0') || !d.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let r: u32 = d.parse().ok()?;
    (r >= 1 && r <= MAX_ROWS).then_some((r, c))
}

pub fn a1(r: u32, c: u32) -> String {
    format!("{}{r}", col_name(c))
}

// ---- references --------------------------------------------------------------------------

fn ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// The names of sheets, by position (lowercase), to resolve `Name!`.
pub struct Sheets<'a> {
    pub names: &'a [String],
}

impl Sheets<'_> {
    fn find(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n.eq_ignore_ascii_case(name))
    }
}

/// A formula with its references replaced by names: the text and the references, or why it
/// cannot be read. `here`: the sheet of the cell; `taken`: the names the model defines, which
/// a bare `Q1` means instead of a cell (`$Q$1` is the cell).
pub fn rewrite(src: &str, here: usize, sheets: &Sheets, taken: &dyn Fn(&str) -> bool) -> Result<(String, Vec<Ref>), (String, &'static str)> {
    let ch: Vec<char> = src.chars().collect();
    let n = ch.len();
    let mut out = String::with_capacity(src.len() + 16);
    let mut refs = Vec::new();
    let mut i = 0;
    while i < n {
        let c = ch[i];
        // strings (and f-strings) are copied as they are
        if c == '"' || (c == 'f' && i + 1 < n && ch[i + 1] == '"' && (i == 0 || !ident_char(ch[i - 1]))) {
            let start = i;
            if c == 'f' {
                i += 1;
            }
            i += 1;
            while i < n && ch[i] != '"' {
                if ch[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i >= n {
                return Err(("a text is not closed: a `\"` is missing".into(), "#ERROR"));
            }
            i += 1;
            out.extend(&ch[start..i]);
            continue;
        }
        if c == '#' {
            if src[byte_at(&ch, i)..].starts_with("#REF!") {
                return Err(("a reference to cells that were deleted (#REF!)".into(), "#REF"));
            }
            // a comment: the rest of the line
            break;
        }
        let boundary = i == 0 || !(ident_char(ch[i - 1]) || ch[i - 1] == '.');
        if boundary {
            if let Some((len, r)) = reference(&ch, i, here, sheets, taken)? {
                out.push_str(&r.name());
                refs.push(r);
                i += len;
                continue;
            }
            // a name: copied whole (a name like `Order2` is not `R2`)
            if c.is_ascii_alphabetic() || c == '_' {
                let start = i;
                while i < n && ident_char(ch[i]) {
                    i += 1;
                }
                out.extend(&ch[start..i]);
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    Ok((out, refs))
}

fn byte_at(ch: &[char], i: usize) -> usize {
    ch[..i].iter().map(|c| c.len_utf8()).sum()
}

/// A reference starting at `i`: its length and what it is; None when there is none there.
fn reference(ch: &[char], i: usize, here: usize, sheets: &Sheets, taken: &dyn Fn(&str) -> bool) -> Result<Option<(usize, Ref)>, (String, &'static str)> {
    let n = ch.len();
    let mut j = i;
    let mut sheet = here;
    let mut prefixed = false;
    // `'My sheet'!` or `Sales!`
    if ch[j] == '\'' {
        let mut name = String::new();
        j += 1;
        loop {
            if j >= n {
                return Ok(None);
            }
            if ch[j] == '\'' {
                if j + 1 < n && ch[j + 1] == '\'' {
                    name.push('\'');
                    j += 2;
                    continue;
                }
                break;
            }
            name.push(ch[j]);
            j += 1;
        }
        j += 1;
        if j >= n || ch[j] != '!' {
            return Ok(None);
        }
        j += 1;
        sheet = sheets.find(&name).ok_or_else(|| (format!("no sheet named `{name}`"), "#REF"))?;
        prefixed = true;
    } else if ch[j].is_ascii_alphabetic() || ch[j] == '_' {
        let mut k = j;
        while k < n && ident_char(ch[k]) {
            k += 1;
        }
        if k < n && ch[k] == '!' && !(k + 1 < n && ch[k + 1] == '=') {
            let name: String = ch[j..k].iter().collect();
            sheet = sheets.find(&name).ok_or_else(|| (format!("no sheet named `{name}`"), "#REF"))?;
            j = k + 1;
            prefixed = true;
        }
    }
    let start = j;
    // a cell (`$A$1`) or the column of a column range (`A:C`)
    let cell = |j: usize| -> Option<(usize, u32, Option<u32>, bool)> {
        let mut k = j;
        let mut dollar = false;
        if k < n && ch[k] == '$' {
            k += 1;
            dollar = true;
        }
        let l0 = k;
        while k < n && ch[k].is_ascii_uppercase() && k - l0 < 4 {
            k += 1;
        }
        let letters: String = ch[l0..k].iter().collect();
        let col = col_index(&letters)?;
        if k < n && ch[k] == '$' {
            k += 1;
            dollar = true;
        }
        let d0 = k;
        while k < n && ch[k].is_ascii_digit() {
            k += 1;
        }
        if d0 == k {
            return Some((k, col, None, dollar));
        }
        if ch[d0] == '0' {
            return None;
        }
        let row: u32 = ch[d0..k].iter().collect::<String>().parse().ok()?;
        (row <= MAX_ROWS).then_some((k, col, Some(row), dollar))
    };
    let Some((k, c1, row1, dollar1)) = cell(j) else {
        return Ok(None);
    };
    let follows_ok = |k: usize| k >= n || !(ident_char(ch[k]) || ch[k] == '(');
    match row1 {
        // `A:C`
        None => {
            if k < n && ch[k] == ':' {
                if let Some((k2, c2, None, _)) = cell(k + 1) {
                    if follows_ok(k2) {
                        let (a, b) = (c1.min(c2), c1.max(c2));
                        return Ok(Some((k2 - i, Ref::Range { s: sheet, r1: 1, c1: a, r2: MAX_ROWS, c2: b })));
                    }
                }
            }
            Ok(None)
        }
        Some(r1) => {
            // `A1:B10`
            if k < n && ch[k] == ':' {
                if let Some((k2, c2, Some(r2), _)) = cell(k + 1) {
                    if follows_ok(k2) {
                        let r = Ref::Range { s: sheet, r1: r1.min(r2), c1: c1.min(c2), r2: r1.max(r2), c2: c1.max(c2) };
                        return Ok(Some((k2 - i, r)));
                    }
                }
            }
            // `A1#`
            if k < n && ch[k] == '#' {
                return Ok(Some((k + 1 - i, Ref::Spill(Addr { s: sheet, r: r1, c: c1 }))));
            }
            if !follows_ok(k) {
                return Ok(None);
            }
            let text: String = ch[start..k].iter().collect();
            if !prefixed && !dollar1 && taken(&text) {
                return Ok(None);
            }
            Ok(Some((k - i, Ref::Cell(Addr { s: sheet, r: r1, c: c1 }))))
        }
    }
}

// ---- inputs ------------------------------------------------------------------------------

/// What a cell holds.
#[derive(Clone, Debug)]
pub enum Input {
    Value(V),
    Formula(String),
}

/// A value as typed in a cell: a number, `5.40 EUR`, `3 pcs`, `10%`, a date, yes or no, or
/// text (`'` first: text whatever follows).
pub fn classify(i: &mut Interp, raw: &str) -> Option<Input> {
    if raw.trim().is_empty() {
        return None;
    }
    if let Some(f) = raw.strip_prefix('=') {
        return Some(Input::Formula(f.to_string()));
    }
    if let Some(t) = raw.strip_prefix('\'') {
        return Some(Input::Value(V::Text(Rc::from(t))));
    }
    let t = raw.trim();
    match t {
        "true" | "TRUE" => return Some(Input::Value(V::Bool(true))),
        "false" | "FALSE" => return Some(Input::Value(V::Bool(false))),
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>() {
        if !t.starts_with('+') {
            return Some(Input::Value(V::Int(n)));
        }
    }
    let lit = t.len() <= 100 && t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-');
    if lit {
        if let Ok(v) = quadra_api::json::literal(i, t) {
            return Some(Input::Value(v.normalized()));
        }
    }
    Some(Input::Value(V::Text(Rc::from(raw))))
}

/// The type of a value as the language writes it, for a parameter (None: no type to give).
pub(super) fn type_text(v: &V) -> Option<String> {
    Some(match v {
        V::Int(_) => "Int".into(),
        V::Dec(_) => "Decimal".into(),
        V::Pct(_) => "Percent".into(),
        V::Text(_) => "Text".into(),
        V::Bool(_) => "Bool".into(),
        V::Date(_) => "Date".into(),
        V::DateTime(_) => "DateTime".into(),
        V::Duration(_) => "Duration".into(),
        V::Money { cur, .. } => format!("Money<{cur}>"),
        V::Qty { unit, .. } => format!("Qty<{unit}>"),
        V::Price { cur, per, .. } => format!("Price<{cur}, {per}>"),
        _ => return None,
    })
}

/// A workbook to evaluate, from a request.
pub struct Book {
    pub names: Vec<String>,
    pub ids: Vec<String>,
    /// per sheet: (row, col) → raw input
    pub cells: Vec<BTreeMap<(u32, u32), String>>,
    /// expressions outside the cells (`{= … }` of a markdown text): key, formula
    pub exprs: Vec<(String, String)>,
    /// a shorter time limit than `DEADLINE_SECS` (milliseconds)
    pub timeout_ms: Option<u64>,
    /// the app's filters and selected records (`params`) and the entities they apply to
    /// (`bind`): engine/params.rs
    pub params: J,
}

impl Book {
    /// The book of a component's configuration, with the app's filters of an export.
    pub fn with_scope(cfg: &J, scope: &J) -> Result<Book, Fail> {
        let mut b = Book::from_json(cfg)?;
        b.params = json!({ "params": scope["params"], "bind": scope["bind"] });
        Ok(b)
    }

    pub fn from_json(body: &J) -> Result<Book, Fail> {
        let bad = |m: String| (400, m);
        let sheets = body["sheets"].as_array().cloned().unwrap_or_default();
        if sheets.len() > MAX_SHEETS {
            return Err(bad(format!("at most {MAX_SHEETS} sheets")));
        }
        let mut book = Book { names: Vec::new(), ids: Vec::new(), cells: Vec::new(), exprs: Vec::new(), timeout_ms: body["timeout_ms"].as_u64(), params: json!({ "params": body["params"], "bind": body["bind"] }) };
        let mut total = 0;
        for (k, s) in sheets.iter().enumerate() {
            let id = s["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("s{}", k + 1));
            let name = s["name"].as_str().map(str::to_string).unwrap_or_else(|| id.clone());
            if book.names.iter().any(|n: &String| n.eq_ignore_ascii_case(&name)) || book.ids.contains(&id) {
                return Err(bad(format!("two sheets named `{name}`")));
            }
            let mut cells = BTreeMap::new();
            if let Some(o) = s["cells"].as_object() {
                total += o.len();
                if total > MAX_CELLS {
                    return Err(bad(format!("at most {MAX_CELLS} cells with something in them")));
                }
                for (addr, c) in o {
                    let raw = match c {
                        J::String(t) => t.clone(),
                        J::Object(_) => match &c["v"] {
                            J::String(t) => t.clone(),
                            J::Number(x) => x.to_string(),
                            J::Bool(b) => b.to_string(),
                            _ => continue,
                        },
                        J::Number(x) => x.to_string(),
                        _ => continue,
                    };
                    if raw.trim().is_empty() {
                        continue;
                    }
                    let (r, c) = parse_a1(addr).ok_or_else(|| bad(format!("`{addr}` is not a cell")))?;
                    cells.insert((r, c), raw);
                }
            }
            book.names.push(name);
            book.ids.push(id);
            book.cells.push(cells);
        }
        if let Some(o) = body["exprs"].as_object() {
            for (k, e) in o.iter().take(200) {
                book.exprs.push((k.clone(), e.as_str().unwrap_or("").to_string()));
            }
        }
        Ok(book)
    }

    /// The sheet and cell of an address in answers: `s1!B2`.
    fn key(&self, a: Addr) -> String {
        format!("{}!{}", self.ids.get(a.s).map(String::as_str).unwrap_or("?"), a1(a.r, a.c))
    }
}

// ---- results -----------------------------------------------------------------------------

/// A cell's result.
#[derive(Clone, Debug)]
pub enum Out {
    Ok(V),
    Err { code: &'static str, msg: String },
}

fn error_code(msg: &str) -> &'static str {
    if msg.contains("is hidden from you") {
        "#HIDDEN"
    } else if msg.contains("you may not read") || msg.contains("permission denied") {
        "#NOACCESS"
    } else if msg.starts_with("timeout") {
        "#TIMEOUT"
    } else {
        "#ERROR"
    }
}

/// A formula's value kept between requests: until a commit changes what it reads.
#[derive(Clone)]
pub struct Kept {
    pub out: Out,
    pub reads: Vec<String>,
    /// the database as of which it holds
    pub tx: u64,
}

/// The engine's memory of sheets: checked workbooks by their source, formulas reading the
/// database by (user, formula, values it uses), and the last transaction each entity changed in.
#[derive(Default)]
pub struct SheetState {
    checks: HashMap<String, Rc<quadra_analysis::Checked>>,
    check_order: VecDeque<String>,
    /// how many workbooks were checked (not found among those kept)
    pub checked: u64,
    pub kept: HashMap<u64, Kept>,
    /// (entity → the last transaction that wrote it) since `seen`
    changed: HashMap<String, u64>,
    /// `changed` holds every write after `start`, up to `seen`
    start: u64,
    seen: u64,
    /// completion (engine/complete.rs): the declarations' docs of the model in use, and what
    /// each user may read as of a transaction
    pub(super) complete_docs: Option<Rc<super::complete::Docs>>,
    pub(super) complete_perms: HashMap<u64, Rc<super::complete::Perms>>,
}

struct Node {
    addr: Addr,
    /// the formula with its references as names; its references
    text: String,
    refs: Vec<Ref>,
}

/// A spill: the cells a formula's list fills, from its anchor.
pub struct Spilled {
    pub rows: Vec<Vec<V>>,
    pub total: usize,
    pub width: usize,
    /// the names of the columns, when the rows are named tuples (`(month: …, total: …)`)
    pub header: Option<Vec<String>>,
}

impl Engine {
    /// `POST /v1/eval`: a workbook's formulas evaluated as the user.
    pub(super) fn sheet_eval(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        let started = std::time::Instant::now();
        let book = Book::from_json(body)?;
        let changed: Option<Vec<String>> = body["changed"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
        let (outs, reads, base, spills) = self.eval_book(who, &book)?;
        // with `changed`: those cells and the cells that use them, through any chain
        let keep: Option<HashSet<Addr>> = changed.map(|list| {
            let mut set: HashSet<Addr> = HashSet::new();
            for k in list {
                if let Some((sid, a)) = k.split_once('!') {
                    if let (Some(s), Some((r, c))) = (book.ids.iter().position(|x| x == sid), parse_a1(a)) {
                        set.insert(Addr { s, r, c });
                    }
                }
            }
            dependents(&outs.1, &spills, &set)
        });
        let mut cells = serde_json::Map::new();
        let mut exprs = serde_json::Map::new();
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        let mut i = self.interp(model, Some(who), None)?;
        for (a, out) in &outs.0 {
            if keep.as_ref().is_some_and(|k| !k.contains(a)) {
                continue;
            }
            let mut j = out_json(&mut i, out);
            if let Some(sp) = spills.get(a) {
                j["spill"] = spill_json(&mut i, sp);
            }
            if let Some(m) = outs.2.get(a) {
                j["certified"] = json!(m);
            }
            if outs.3.contains(a) {
                j["db"] = json!(true);
            }
            if a.s >= book.names.len() {
                // an expression's list: its rows as a spill (a chart's series, a KPI's sparkline)
                if let Out::Ok(v) = out {
                    if let Some(sp) = spill_of(v) {
                        j["spill"] = spill_json(&mut i, &sp);
                    }
                }
                let key = book.exprs.get((a.r - 1) as usize).map(|e| e.0.clone()).unwrap_or_default();
                exprs.insert(key, j);
            } else {
                cells.insert(book.key(*a), j);
            }
        }
        let mut reads: Vec<String> = reads.into_iter().collect();
        reads.sort();
        Ok(reply(200, json!({ "base": base, "ms": started.elapsed().as_millis() as u64, "reads": reads, "partial": keep.is_some(), "cells": cells, "exprs": exprs })))
    }

    /// The formulas of a workbook evaluated as the user: (results, the formulas each one
    /// uses, certified metrics, the formulas reading the database), the entities read, the
    /// transaction, the spills.
    #[allow(clippy::type_complexity)]
    pub(super) fn eval_book(&mut self, who: &Principal, book: &Book) -> Result<((BTreeMap<Addr, Out>, Vec<(Addr, Vec<Ref>)>, HashMap<Addr, String>, HashSet<Addr>), HashSet<String>, u64, HashMap<Addr, Spilled>), Fail> {
        let limit = book.timeout_ms.map_or(DEADLINE_SECS * 1000, |t| t.min(DEADLINE_SECS * 1000));
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(limit);
        let checked = self.checked.clone();
        let model = checked.model.as_ref().unwrap();
        // what the cells hold
        let mut values: HashMap<Addr, V> = HashMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut outs: BTreeMap<Addr, Out> = BTreeMap::new();
        let params;
        {
            let mut i = self.interp(model, Some(who), None)?;
            params = super::params::params_of(&mut i, model, &book.params)?;
            let taken = |n: &str| {
                model.entity(n).is_some()
                    || model.variant(n).is_some()
                    || model.function(n).is_some()
                    || model.enum_def(n).is_some()
                    || model.consts.iter().any(|c| c.name == n)
                    || model.metrics.iter().any(|m| m.name == n)
            };
            let sheets = Sheets { names: &book.names };
            let mut formulas = 0;
            let all = book.cells.iter().enumerate().flat_map(|(s, cells)| cells.iter().map(move |((r, c), raw)| (Addr { s, r: *r, c: *c }, raw.clone())));
            let extra = book.exprs.iter().enumerate().map(|(k, (_, e))| (Addr { s: book.names.len(), r: k as u32 + 1, c: 1 }, format!("={e}")));
            for (a, raw) in all.chain(extra) {
                match classify(&mut i, &raw) {
                    None => {}
                    Some(Input::Value(v)) => {
                        values.insert(a, v);
                    }
                    Some(Input::Formula(f)) => {
                        formulas += 1;
                        if formulas > MAX_FORMULAS {
                            return Err((400, format!("at most {MAX_FORMULAS} formulas")));
                        }
                        let here = if a.s >= book.names.len() { 0 } else { a.s };
                        if f.len() > MAX_FORMULA || f.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                            outs.insert(a, Out::Err { code: "#ERROR", msg: format!("a formula is one line of at most {MAX_FORMULA} characters") });
                            continue;
                        }
                        let f = match super::params::rewrite_params(&f, &params.list) {
                            Ok(f) => f,
                            Err(msg) => {
                                outs.insert(a, Out::Err { code: "#REF", msg });
                                continue;
                            }
                        };
                        match rewrite(&f, here, &sheets, &taken) {
                            Ok((text, refs)) => {
                                let text = super::params::rewrite_scopes(&text, &params.scopes);
                                if text.trim().is_empty() {
                                    outs.insert(a, Out::Err { code: "#ERROR", msg: "an empty formula".into() });
                                    continue;
                                }
                                // a formula the parser reads on its own, as one expression
                                let p = quadra_syntax::parse(&format!("fn __f():\n    let __x = {text}\n"));
                                if let Some(e) = p.errors.first() {
                                    outs.insert(a, Out::Err { code: "#ERROR", msg: e.message.clone() });
                                    continue;
                                }
                                nodes.push(Node { addr: a, text, refs });
                            }
                            Err((msg, code)) => {
                                outs.insert(a, Out::Err { code, msg });
                            }
                        }
                    }
                }
            }
        }
        let index: HashMap<Addr, usize> = nodes.iter().enumerate().map(|(k, n)| (n.addr, k)).collect();
        // formulas that cannot be read: those using them have their error
        let pre_err: BTreeMap<Addr, Out> = outs.clone();
        // the formulas each formula uses (directly; ranges: the formulas inside them)
        let mut uses: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (k, n) in nodes.iter().enumerate() {
            let mut deps = Vec::new();
            for r in &n.refs {
                match r {
                    Ref::Cell(a) | Ref::Spill(a) => {
                        if let Some(&d) = index.get(a) {
                            deps.push(d);
                        }
                    }
                    Ref::Range { s, r1, c1, r2, c2 } => {
                        if let Some(cells) = book.cells.get(*s) {
                            for ((r, c), _) in cells.range((*r1, 0)..=(*r2, u32::MAX)) {
                                if c >= c1 && c <= c2 {
                                    if let Some(&d) = index.get(&Addr { s: *s, r: *r, c: *c }) {
                                        deps.push(d);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            deps.sort();
            deps.dedup();
            uses[k] = deps;
        }
        // the order of evaluation; the formulas left are on a cycle or use one
        let order = topo(&uses);
        let placed: HashSet<usize> = order.iter().copied().collect();
        let left: Vec<usize> = (0..nodes.len()).filter(|k| !placed.contains(k)).collect();
        for &k in &left {
            let cycle = on_cycle(k, &uses, &placed);
            let out = match cycle {
                Some(path) => Out::Err { code: "#CYCLE", msg: format!("circular reference: {}", path.iter().map(|&x| a1(nodes[x].addr.r, nodes[x].addr.c)).collect::<Vec<_>>().join(" → ")) },
                None => Out::Err { code: "#CYCLE", msg: "uses a cell of a circular reference".into() },
            };
            outs.insert(nodes[k].addr, out);
        }
        // evaluation, in rounds: a formula using cells another one spilled into is evaluated
        // again once the spill is known
        let mut spills: HashMap<Addr, Spilled> = HashMap::new();
        let mut area: HashMap<Addr, (Addr, usize, usize)> = HashMap::new();
        let mut results: HashMap<usize, Out> = HashMap::new();
        let mut reads_of: HashMap<usize, Vec<String>> = HashMap::new();
        let mut certified: HashMap<Addr, String> = HashMap::new();
        let mut db: HashSet<Addr> = HashSet::new();
        let mut base = 0;
        let mut bad_types: HashMap<usize, (String, &'static str)> = HashMap::new();
        let mut demoted: HashSet<Addr> = HashSet::new();
        // formulas whose spill is blocked (their value is kept for the next round's spills)
        let mut blocked: HashMap<usize, String> = HashMap::new();
        let mut raw: HashMap<usize, V> = HashMap::new();
        for round in 0..4 {
            // the source: parameters for values and for what is known only when evaluating
            let mut tries = 0;
            let (over, plan) = loop {
                let plan = Plan::build(&nodes, &order, &values, &area, &bad_types, &demoted, book, &params.list);
                let over = self.sheet_check(&plan.source)?;
                // a value whose type the model does not know (`3 XYZ`): text, and again
                let mut again = false;
                for (line, msg) in &over.1 {
                    match plan.lines.get(*line) {
                        Some(Line::Param(a)) if values.contains_key(a) && !demoted.contains(a) => {
                            demoted.insert(*a);
                            again = true;
                        }
                        Some(Line::Formula(k)) => {
                            if !bad_types.contains_key(k) {
                                bad_types.insert(*k, (msg.clone(), "#TYPE"));
                                again = true;
                            }
                        }
                        _ => {}
                    }
                }
                tries += 1;
                if !again || tries > 4 {
                    break (over.0, plan);
                }
            };
            let Some(over) = over else {
                return Err((500, "the workbook does not check".into()));
            };
            let omodel = over.model.as_ref().unwrap();
            let Some(f) = omodel.function("__sheet") else {
                return Err((500, "the workbook does not check".into()));
            };
            let lets: HashMap<String, &Expr> = f
                .body
                .iter()
                .flatten()
                .filter_map(|s| match &s.kind {
                    StmtKind::Let { name, value, .. } => Some((name.clone(), value)),
                    _ => None,
                })
                .collect();
            let mut i = self.interp(omodel, Some(who), None)?;
            base = i.store.base;
            i.viewer = true;
            // the workbook's limit, within the request's own
            i.deadline = Some(i.deadline.map_or(deadline, |d| d.min(deadline)));
            let mut ctx = Ctx::default();
            ctx.scopes.push(Default::default());
            // the app's filters and selected records
            for p in &params.list {
                ctx.bind(&p.var, p.value.clone());
            }
            // parameters
            for p in &plan.params {
                let v = match p {
                    Ref::Cell(a) => match values.get(a) {
                        Some(v) if !demoted.contains(a) => v.clone(),
                        Some(_) => V::Text(Rc::from(book.cells.get(a.s).and_then(|c| c.get(&(a.r, a.c))).map(String::as_str).unwrap_or(""))),
                        None => match area.get(a) {
                            Some((anchor, dr, dc)) => spills.get(anchor).and_then(|sp| sp.rows.get(*dr)).and_then(|row| row.get(*dc)).cloned().unwrap_or(V::None),
                            None => index
                                .get(a)
                                .and_then(|k| match results.get(k) {
                                    Some(Out::Ok(v)) => Some(v.clone()),
                                    _ => None,
                                })
                                .unwrap_or(V::None),
                        },
                    },
                    Ref::Range { s, r1, c1, r2, c2 } => {
                        let mut items = Vec::new();
                        let mut cells: Vec<(u32, u32)> = book.cells.get(*s).map(|cs| cs.range((*r1, 0)..=(*r2, u32::MAX)).filter(|((_, c), _)| c >= c1 && c <= c2).map(|(k, _)| *k).collect()).unwrap_or_default();
                        for (a, _) in area.iter().filter(|(a, _)| a.s == *s && a.r >= *r1 && a.r <= *r2 && a.c >= *c1 && a.c <= *c2) {
                            cells.push((a.r, a.c));
                        }
                        cells.sort();
                        cells.dedup();
                        for (r, c) in cells {
                            let a = Addr { s: *s, r, c };
                            let v = if let Some(v) = values.get(&a) {
                                v.clone()
                            } else if let Some(k) = index.get(&a) {
                                match results.get(k) {
                                    Some(Out::Ok(v)) => v.clone(),
                                    _ => V::None,
                                }
                            } else if let Some((anchor, dr, dc)) = area.get(&a) {
                                spills.get(anchor).and_then(|sp| sp.rows.get(*dr)).and_then(|row| row.get(*dc)).cloned().unwrap_or(V::None)
                            } else {
                                V::None
                            };
                            if !v.is_none() {
                                items.push(v);
                            }
                        }
                        V::List(Rc::new(items))
                    }
                    Ref::Spill(a) => index
                        .get(a)
                        .and_then(|k| match results.get(k) {
                            Some(Out::Ok(v)) => Some(v.clone()),
                            _ => None,
                        })
                        .unwrap_or(V::None),
                };
                ctx.bind(&p.name(), v);
            }
            // formulas that do not check: their error
            for (k, (msg, code)) in &bad_types {
                results.insert(*k, Out::Err { code, msg: msg.clone() });
            }
            // the formulas, in order
            for step in &plan.steps {
                match step {
                    Step::Range(name) | Step::Spill(name) => {
                        if let Some(e) = lets.get(name) {
                            let v = i.eval(e, &mut ctx).unwrap_or(V::None);
                            ctx.bind(name, v);
                        }
                    }
                    Step::Formula(k) => {
                        let k = *k;
                        let node = &nodes[k];
                        let name = Ref::Cell(node.addr).name();
                        // a formula using one in error has its error
                        let failed = uses[k]
                            .iter()
                            .find_map(|d| match results.get(d) {
                                Some(Out::Err { code, .. }) => Some((nodes[*d].addr, *code)),
                                _ => None,
                            })
                            .or_else(|| {
                                node.refs.iter().find_map(|r| match r {
                                    Ref::Cell(a) | Ref::Spill(a) => match pre_err.get(a) {
                                        Some(Out::Err { code, .. }) => Some((*a, *code)),
                                        _ => None,
                                    },
                                    _ => None,
                                })
                            });
                        let out = if let Some((msg, code)) = bad_types.get(&k) {
                            Out::Err { code, msg: msg.clone() }
                        } else if let Some((d, code)) = failed {
                            Out::Err { code, msg: format!("uses {}, which has an error", a1(d.r, d.c)) }
                        } else if let Some(e) = lets.get(&name) {
                            if let Some(m) = metric_of(e, omodel) {
                                certified.insert(node.addr, m);
                            }
                            let reads = expr_reads(e, omodel, &self.schema);
                            if !reads.is_empty() {
                                db.insert(node.addr);
                            }
                            let mut out = self.eval_cell(&mut i, e, &mut ctx, node, &reads, who.user_id, base, &params.list);
                            reads_of.insert(k, reads);
                            if let Out::Ok(v) = &out {
                                raw.insert(k, v.clone());
                                // its spill is blocked: those using it see the error
                                if let Some(msg) = blocked.get(&k) {
                                    out = Out::Err { code: "#SPILL", msg: msg.clone() };
                                }
                            }
                            out
                        } else {
                            Out::Err { code: "#ERROR", msg: "internal: a formula was not checked".into() }
                        };
                        ctx.bind(
                            &name,
                            match &out {
                                Out::Ok(v) => v.clone(),
                                _ => V::None,
                            },
                        );
                        results.insert(k, out);
                    }
                }
            }
            drop(i);
            // the spills, and the cells they fill
            let before: HashSet<Addr> = area.keys().copied().collect();
            spills.clear();
            area.clear();
            let mut newly = false;
            for &k in &order {
                let a = nodes[k].addr;
                if a.s >= book.names.len() {
                    continue;
                }
                let v = match results.get(&k) {
                    Some(Out::Ok(v)) => v.clone(),
                    Some(Out::Err { code: "#SPILL", .. }) => match raw.get(&k) {
                        Some(v) => v.clone(),
                        None => continue,
                    },
                    _ => continue,
                };
                let Some(sp) = spill_of(&v) else { continue };
                let (h, w) = (sp.rows.len(), sp.width);
                // blocked by a cell holding something, or by another spill
                let mut block_at = None;
                'scan: for dr in 0..h {
                    for dc in 0..w {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let b = Addr { s: a.s, r: a.r + dr as u32, c: a.c + dc as u32 };
                        if b.r > MAX_ROWS || b.c > MAX_COLS {
                            block_at = Some("the edge of the sheet".to_string());
                            break 'scan;
                        }
                        if book.cells[a.s].contains_key(&(b.r, b.c)) || area.contains_key(&b) {
                            block_at = Some(a1(b.r, b.c));
                            break 'scan;
                        }
                    }
                }
                if let Some(at) = block_at {
                    let msg = format!("the result ({h} rows × {w} columns) would overwrite {at}: clear those cells");
                    if blocked.insert(k, msg.clone()).as_ref() != Some(&msg) {
                        newly = true;
                    }
                    results.insert(k, Out::Err { code: "#SPILL", msg });
                    continue;
                }
                if blocked.remove(&k).is_some() {
                    newly = true;
                    results.insert(k, Out::Ok(v.clone()));
                }
                for dr in 0..h {
                    for dc in 0..w {
                        if dr + dc > 0 {
                            area.insert(Addr { s: a.s, r: a.r + dr as u32, c: a.c + dc as u32 }, (a, dr, dc));
                        }
                    }
                }
                spills.insert(a, sp);
            }
            // another round when a formula uses a cell a spill fills (new or moved)
            let now: HashSet<Addr> = area.keys().copied().collect();
            let uses_spill = nodes.iter().any(|n| {
                n.refs.iter().any(|r| match r {
                    Ref::Cell(a) => now.contains(a) || before.contains(a),
                    Ref::Range { s, r1, c1, r2, c2 } => now.iter().chain(before.iter()).any(|a| a.s == *s && a.r >= *r1 && a.r <= *r2 && a.c >= *c1 && a.c <= *c2),
                    Ref::Spill(_) => false,
                })
            });
            if !newly && (!uses_spill || (round > 0 && now == before)) {
                break;
            }
        }
        let mut all_reads = HashSet::new();
        for r in reads_of.values() {
            all_reads.extend(r.iter().cloned());
        }
        for (k, out) in results {
            outs.insert(nodes[k].addr, out);
        }
        // the formulas with their references, those using others after them
        let mut graph: Vec<(Addr, Vec<Ref>)> = order.iter().map(|&k| (nodes[k].addr, nodes[k].refs.clone())).collect();
        graph.extend(left.iter().map(|&k| (nodes[k].addr, nodes[k].refs.clone())));
        Ok(((outs, graph, certified, db), all_reads, base, spills))
    }

    /// A formula's value: kept from an earlier request while nothing it reads changed.
    #[allow(clippy::too_many_arguments)]
    fn eval_cell(&mut self, i: &mut Interp, e: &Expr, ctx: &mut Ctx, node: &Node, reads: &[String], user: u64, base: u64, params: &[super::params::Param]) -> Out {
        let key = if reads.is_empty() {
            None
        } else {
            // the formula, the user, the model, and the values of the names it uses
            let mut h = std::collections::hash_map::DefaultHasher::new();
            node.text.hash(&mut h);
            user.hash(&mut h);
            self.model_key().hash(&mut h);
            for r in &node.refs {
                format!("{:?}", ctx.local(&r.name())).hash(&mut h);
            }
            // the filters it uses: a formula is kept per value of its filters
            for p in params.iter().filter(|p| node.text.contains(p.var.as_str())) {
                p.var.hash(&mut h);
                format!("{:?}", p.value).hash(&mut h);
            }
            Some(h.finish())
        };
        if let Some(k) = key {
            if self.sheet_fresh(k, base) {
                if let Some(kept) = self.sheets.kept.get(&k) {
                    return kept.out.clone();
                }
            }
        }
        let out = match i.eval(e, ctx) {
            Ok(v) => Out::Ok(v),
            Err(f) => {
                let msg = quadra_api::json::describe(f);
                Out::Err { code: error_code(&msg), msg }
            }
        };
        if let Some(k) = key {
            // a timeout is not kept: the next request may be luckier
            if !matches!(&out, Out::Err { code: "#TIMEOUT", .. }) {
                if self.sheets.kept.len() > 20_000 {
                    self.sheets.kept.clear();
                }
                self.sheets.kept.insert(k, Kept { out: out.clone(), reads: reads.to_vec(), tx: base });
            }
        }
        out
    }

    /// Whether a kept value still holds at `base`: no commit since it was computed wrote an
    /// entity it reads.
    fn sheet_fresh(&mut self, key: u64, base: u64) -> bool {
        let Some(kept) = self.sheets.kept.get(&key) else {
            return false;
        };
        if kept.tx == base {
            return true;
        }
        if kept.tx > base {
            return false;
        }
        let since = kept.tx;
        let reads = kept.reads.clone();
        if !self.sheet_changes(since, base) {
            return false;
        }
        let stale = reads.iter().any(|e| self.sheets.changed.get(e).is_some_and(|t| *t > since));
        if stale {
            self.sheets.kept.remove(&key);
            return false;
        }
        if let Some(k) = self.sheets.kept.get_mut(&key) {
            k.tx = base;
        }
        true
    }

    /// The entities written by the transactions after `since` up to `base` are known
    /// (`changed`); false when they cannot be told (a value kept before the engine looked).
    fn sheet_changes(&mut self, since: u64, base: u64) -> bool {
        use quadra_store::conn::Sql;
        if self.sheets.start == 0 {
            self.sheets.start = base;
            self.sheets.seen = base;
        }
        if since < self.sheets.start {
            return false;
        }
        if base > self.sheets.seen {
            let from = self.sheets.seen;
            if base - from > 5000 {
                // too much happened: start again from here
                let st = &mut self.sheets;
                st.start = base;
                st.seen = base;
                st.changed.clear();
                st.kept.clear();
                return false;
            }
            let rows = match self.conn.query(
                "SELECT r.\"entity\", CAST(MAX(h.\"tx\") AS TEXT) FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE h.\"tx\" > ?bigint AND h.\"tx\" <= ?bigint GROUP BY r.\"entity\"",
                &[Sql::Int(from as i64), Sql::Int(base as i64)],
            ) {
                Ok(r) => r,
                Err(_) => return false,
            };
            let st = &mut self.sheets;
            for r in rows {
                if let (Some(e), Some(t)) = (r[0].clone(), r[1].as_deref().and_then(|t| t.parse::<u64>().ok())) {
                    let cur = st.changed.entry(e).or_insert(0);
                    *cur = (*cur).max(t);
                }
            }
            st.seen = base;
        }
        true
    }

    /// The workbook's function checked with the project: the checked project (None when it
    /// does not check at all) and the errors by line of the source.
    #[allow(clippy::type_complexity)]
    fn sheet_check(&mut self, src: &str) -> Result<(Option<Rc<quadra_analysis::Checked>>, Vec<(usize, String)>), Fail> {
        let path = self.root.join("tests/__sheet__.qd");
        let c = match self.sheets.checks.get(src) {
            Some(c) => c.clone(),
            None => {
                self.sheets.checked += 1;
                let mut sources = self.files.clone();
                sources.push((path.clone(), src.to_string()));
                let project = Project::from_sources(&self.root, sources);
                let c = Rc::new(quadra_analysis::check(&project));
                self.sheets.checks.insert(src.to_string(), c.clone());
                self.sheets.check_order.push_back(src.to_string());
                if self.sheets.check_order.len() > 16 {
                    if let Some(old) = self.sheets.check_order.pop_front() {
                        self.sheets.checks.remove(&old);
                    }
                }
                c
            }
        };
        let project_file = c.model.as_ref().and_then(|m| m.sources.iter().position(|(p, _)| p.ends_with("__sheet__.qd")));
        let mut errors = Vec::new();
        for d in c.diagnostics.iter().filter(|d| d.severity == quadra_sema::Severity::Error) {
            let line = if Some(d.file.0 as usize) == project_file || project_file.is_none() {
                let from = usize::from(d.range.start());
                if from <= src.len() {
                    src[..from].matches('\n').count()
                } else {
                    usize::MAX
                }
            } else {
                usize::MAX
            };
            errors.push((line, d.message.clone()));
        }
        // an error elsewhere than in the workbook: the code itself does not check
        if c.model.is_none() {
            return Ok((None, errors));
        }
        Ok((Some(c), errors))
    }
}

/// A line of the generated source.
enum Line {
    Other,
    Param(Addr),
    Formula(usize),
}

enum Step {
    Formula(usize),
    Range(String),
    Spill(String),
}

/// The source of a workbook's function, and how to evaluate it.
struct Plan {
    source: String,
    lines: Vec<Line>,
    params: Vec<Ref>,
    steps: Vec<Step>,
}

impl Plan {
    #[allow(clippy::too_many_arguments)]
    fn build(nodes: &[Node], order: &[usize], values: &HashMap<Addr, V>, area: &HashMap<Addr, (Addr, usize, usize)>, bad: &HashMap<usize, (String, &'static str)>, demoted: &HashSet<Addr>, book: &Book, app_params: &[super::params::Param]) -> Plan {
        let index: HashMap<Addr, usize> = nodes.iter().enumerate().map(|(k, n)| (n.addr, k)).collect();
        let in_order: HashSet<usize> = order.iter().copied().filter(|k| !bad.contains_key(k)).collect();
        let mut params: Vec<(Ref, Option<String>)> = Vec::new();
        let mut seen: HashSet<Ref> = HashSet::new();
        let mut lets: Vec<(String, String, Line, Step)> = Vec::new();
        let mut defined: HashSet<String> = HashSet::new();
        let param = |r: Ref, ty: Option<String>, params: &mut Vec<(Ref, Option<String>)>, seen: &mut HashSet<Ref>| {
            if seen.insert(r.clone()) {
                params.push((r, ty));
            }
        };
        for &k in order {
            if !in_order.contains(&k) {
                continue;
            }
            let n = &nodes[k];
            for r in &n.refs {
                match r {
                    Ref::Cell(a) => {
                        if index.get(a).is_some_and(|d| in_order.contains(d)) {
                            continue;
                        }
                        let ty = values.get(a).filter(|_| !demoted.contains(a)).and_then(type_text).or_else(|| demoted.contains(a).then(|| "Text".to_string()));
                        param(r.clone(), ty, &mut params, &mut seen);
                    }
                    Ref::Spill(a) => {
                        let name = r.name();
                        if index.get(a).is_some_and(|d| in_order.contains(d)) {
                            if defined.insert(name.clone()) {
                                lets.push((name.clone(), Ref::Cell(*a).name(), Line::Other, Step::Spill(name)));
                            }
                        } else {
                            param(r.clone(), None, &mut params, &mut seen);
                        }
                    }
                    Ref::Range { s, r1, c1, r2, c2 } => {
                        let name = r.name();
                        if defined.contains(&name) || seen.contains(r) {
                            continue;
                        }
                        // the cells: typed when few, all known, none filled by a spill
                        let cells: Vec<(u32, u32)> = book.cells.get(*s).map(|cs| cs.range((*r1, 0)..=(*r2, u32::MAX)).filter(|((_, c), _)| c >= c1 && c <= c2).map(|(k, _)| *k).collect()).unwrap_or_default();
                        let spilled = area.keys().any(|a| a.s == *s && a.r >= *r1 && a.r <= *r2 && a.c >= *c1 && a.c <= *c2);
                        let typed = !spilled && cells.len() <= TYPED_RANGE && *r2 != MAX_ROWS;
                        if !typed {
                            param(r.clone(), None, &mut params, &mut seen);
                            continue;
                        }
                        let mut items = Vec::new();
                        for (rr, cc) in cells {
                            let a = Addr { s: *s, r: rr, c: cc };
                            let cr = Ref::Cell(a);
                            match index.get(&a) {
                                Some(d) if in_order.contains(d) => {}
                                Some(_) => {
                                    // a formula in error: unknown, empty
                                    param(cr.clone(), None, &mut params, &mut seen);
                                }
                                None => {
                                    let ty = values.get(&a).filter(|_| !demoted.contains(&a)).and_then(type_text).or_else(|| demoted.contains(&a).then(|| "Text".to_string()));
                                    param(cr.clone(), ty, &mut params, &mut seen);
                                }
                            }
                            items.push(cr.name());
                        }
                        defined.insert(name.clone());
                        lets.push((name.clone(), format!("[{}]", items.join(", ")), Line::Other, Step::Range(name)));
                    }
                }
            }
            lets.push((Ref::Cell(n.addr).name(), n.text.clone(), Line::Formula(k), Step::Formula(k)));
        }
        // formulas not in the function (errors, cycles) are parameters of those using them
        let mut src = String::from("fn __sheet(\n");
        let mut lines = vec![Line::Other];
        // the app's filters: typed by what they filter, not by their values
        for p in app_params {
            match &p.ty {
                Some(t) => src.push_str(&format!("    {}: {t},\n", p.var)),
                None => src.push_str(&format!("    {},\n", p.var)),
            }
            lines.push(Line::Other);
        }
        for (r, ty) in &params {
            match ty {
                Some(t) => src.push_str(&format!("    {}: {t},\n", r.name())),
                None => src.push_str(&format!("    {},\n", r.name())),
            }
            lines.push(match r {
                Ref::Cell(a) => Line::Param(*a),
                _ => Line::Other,
            });
        }
        src.push_str("):\n");
        lines.push(Line::Other);
        let mut steps = Vec::new();
        for (name, text, line, step) in lets {
            src.push_str(&format!("    let {name} = {text}\n"));
            lines.push(line);
            steps.push(step);
        }
        src.push_str("    return none\n");
        Plan { source: src, lines, params: params.into_iter().map(|p| p.0).collect(), steps }
    }
}

/// Kahn's order of the formulas by what they use (those on a cycle, or using one, are left out).
fn topo(uses: &[Vec<usize>]) -> Vec<usize> {
    let n = uses.len();
    let mut indeg = vec![0usize; n];
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, ds) in uses.iter().enumerate() {
        for &d in ds {
            if d == k {
                indeg[k] += 1;
                continue;
            }
            indeg[k] += 1;
            users[d].push(k);
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|k| indeg[*k] == 0).collect();
    let mut out = Vec::with_capacity(n);
    while let Some(k) = queue.pop_front() {
        out.push(k);
        for &u in &users[k] {
            indeg[u] -= 1;
            if indeg[u] == 0 {
                queue.push_back(u);
            }
        }
    }
    out
}

/// A cycle through `k` among the formulas not placed: the path, or None when `k` only uses one.
fn on_cycle(k: usize, uses: &[Vec<usize>], placed: &HashSet<usize>) -> Option<Vec<usize>> {
    let mut prev: HashMap<usize, usize> = HashMap::new();
    let mut queue = VecDeque::from([k]);
    let mut steps = 0;
    while let Some(x) = queue.pop_front() {
        steps += 1;
        if steps > 20_000 {
            return Some(vec![k, k]);
        }
        for &d in &uses[x] {
            if placed.contains(&d) {
                continue;
            }
            if d == k {
                let mut path = vec![k];
                let mut cur = x;
                while cur != k {
                    path.push(cur);
                    cur = prev[&cur];
                }
                path.push(k);
                path.reverse();
                return Some(path);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(d) {
                e.insert(x);
                queue.push_back(d);
            }
        }
    }
    None
}

/// The cells using any of `set`, through any chain (directly, in a range, in a spill), with
/// `set`. `graph`: the formulas, each after those it uses.
fn dependents(graph: &[(Addr, Vec<Ref>)], spills: &HashMap<Addr, Spilled>, set: &HashSet<Addr>) -> HashSet<Addr> {
    let mut out = set.clone();
    // the rectangles of the spills of affected formulas
    let rect = |a: &Addr| spills.get(a).map(|sp| (a.s, a.r, a.c, a.r + sp.rows.len() as u32 - 1, a.c + sp.width as u32 - 1));
    let mut rects: Vec<(usize, u32, u32, u32, u32)> = set.iter().filter_map(rect).collect();
    for (a, refs) in graph {
        if out.contains(a) {
            if let Some(r) = rect(a) {
                rects.push(r);
            }
            continue;
        }
        let inside = |s: usize, r: u32, c: u32| rects.iter().any(|(rs, r1, c1, r2, c2)| *rs == s && r >= *r1 && r <= *r2 && c >= *c1 && c <= *c2);
        let hit = refs.iter().any(|r| match r {
            Ref::Cell(x) => out.contains(x) || inside(x.s, x.r, x.c),
            Ref::Spill(x) => out.contains(x),
            Ref::Range { s, r1, c1, r2, c2 } => {
                out.iter().any(|x| x.s == *s && x.r >= *r1 && x.r <= *r2 && x.c >= *c1 && x.c <= *c2) || rects.iter().any(|(rs, a1, b1, a2, b2)| rs == s && a1 <= r2 && a2 >= r1 && b1 <= c2 && b2 >= c1)
            }
        });
        if hit {
            out.insert(*a);
            if let Some(r) = rect(a) {
                rects.push(r);
            }
        }
    }
    out
}

/// The metric a formula is, when it is one call of a metric (shown as certified).
fn metric_of(e: &Expr, m: &Model) -> Option<String> {
    if let ExprKind::Call { callee, .. } = &e.kind {
        if let ExprKind::Name(n) = &callee.kind {
            if m.metrics.iter().any(|x| x.name == *n && !x.control) {
                return Some(n.clone());
            }
        }
    }
    None
}

/// The entities a formula may read: those it names, those the functions and metrics it calls
/// name, and the entities reachable from them through references (a path `o.customer.name`
/// reads customers). Empty: it does not read the database.
fn expr_reads(e: &Expr, m: &Model, schema: &quadra_store::schema::Schema) -> Vec<String> {
    let mut named: HashSet<String> = HashSet::new();
    let mut fns_seen: HashSet<String> = HashSet::new();
    fn walk(e: &Expr, m: &Model, named: &mut HashSet<String>, fns: &mut HashSet<String>, depth: usize) {
        match &e.kind {
            ExprKind::Name(n) => {
                if m.entity(n).is_some() {
                    named.insert(n.clone());
                }
                if n == "me" || n == "current" {
                    named.insert("User".into());
                }
                if depth < 6 && fns.insert(n.clone()) {
                    if let Some(f) = m.function(n) {
                        for s in f.body.iter().flatten() {
                            stmt_exprs(s, &mut |x| walk(x, m, named, fns, depth + 1));
                        }
                    }
                    if let Some(mt) = m.metrics.iter().find(|x| x.name == *n) {
                        for (_, x) in &mt.formulas {
                            walk(x, m, named, fns, depth + 1);
                        }
                    }
                }
            }
            ExprKind::Block(stmts) => {
                for s in stmts {
                    stmt_exprs(s, &mut |x| walk(x, m, named, fns, depth));
                }
            }
            _ => e.for_each_child(&mut |c| walk(c, m, named, fns, depth)),
        }
    }
    walk(e, m, &mut named, &mut fns_seen, 0);
    if named.is_empty() {
        return Vec::new();
    }
    // through references
    let mut out: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = named.into_iter().collect();
    while let Some(n) = queue.pop_front() {
        if !out.insert(n.clone()) {
            continue;
        }
        if let Some(en) = m.entity(&n) {
            for f in m.all_fields(en) {
                let mut t = f.ty.as_ref();
                while let Some(quadra_ir::Ty::Optional(i) | quadra_ir::Ty::List(i) | quadra_ir::Ty::Set(i)) = t {
                    t = Some(i);
                }
                if let Some(quadra_ir::Ty::Path { segments, .. }) = t {
                    if segments.len() == 1 && m.entity(&segments[0]).is_some() {
                        queue.push_back(segments[0].clone());
                    }
                }
            }
            if en.kind == EntityKind::Trait {
                for x in m.implementors(&n) {
                    queue.push_back(x.name.clone());
                }
            }
        }
    }
    let _ = schema;
    // who may read what may depend on other records (the user's roles, a team…)
    out.insert("User".into());
    for r in m.rules.iter().chain(m.roles.iter().flat_map(|r| r.rules.iter())) {
        if let Some(c) = &r.condition {
            let mut more = HashSet::new();
            walk(c, m, &mut more, &mut HashSet::new(), 6);
            out.extend(more);
        }
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    v
}

fn stmt_exprs(s: &quadra_ir::Stmt, f: &mut dyn FnMut(&Expr)) {
    match &s.kind {
        StmtKind::Let { value, .. } => f(value),
        StmtKind::Return(Some(e)) | StmtKind::Expr(e) | StmtKind::Fail(e) | StmtKind::Expect(e) | StmtKind::Emit(e) => f(e),
        StmtKind::If { cond, then, otherwise, .. } => {
            f(cond);
            for x in then {
                stmt_exprs(x, f);
            }
            for x in otherwise.iter().flatten() {
                stmt_exprs(x, f);
            }
        }
        StmtKind::For { iter, body, .. } => {
            f(iter);
            for x in body {
                stmt_exprs(x, f);
            }
        }
        StmtKind::Require { cond, message } => {
            f(cond);
            if let Some(m) = message {
                f(m);
            }
        }
        _ => {}
    }
}

/// The cells a value fills: a list (one row per element; a tuple or a list per row fills
/// columns), a tuple (one row). None: one cell.
pub fn spill_of(v: &V) -> Option<Spilled> {
    let row = |x: &V| -> Vec<V> {
        match x {
            V::Tuple(items) => items.iter().map(|(_, v)| v.clone()).take(SPILL_COLS).collect(),
            V::List(items) | V::Set(items) => items.iter().take(SPILL_COLS).cloned().collect(),
            other => vec![other.clone()],
        }
    };
    let (rows, total) = match v {
        V::List(items) | V::Set(items) => (items.iter().take(SPILL_ROWS).map(row).collect::<Vec<_>>(), items.len()),
        V::Tuple(_) => (vec![row(v)], 1),
        _ => return None,
    };
    let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let rows = if rows.is_empty() { vec![vec![V::None]] } else { rows };
    let first = match v {
        V::List(items) | V::Set(items) => items.first(),
        t @ V::Tuple(_) => Some(t),
        _ => None,
    };
    let header = match first {
        Some(V::Tuple(items)) if items.iter().all(|(n, _)| n.is_some()) => Some(items.iter().take(SPILL_COLS).map(|(n, _)| n.clone().unwrap_or_default()).collect()),
        _ => None,
    };
    Some(Spilled { rows, total, width, header })
}

fn kind_name(v: &V) -> &'static str {
    match v {
        V::None => "none",
        V::Bool(_) => "bool",
        V::Int(_) => "int",
        V::Dec(_) => "decimal",
        V::Pct(_) => "percent",
        V::Text(_) => "text",
        V::Date(_) => "date",
        V::DateTime(_) => "datetime",
        V::Duration(_) => "duration",
        V::YearMonth(..) => "month",
        V::DateRange(..) => "range",
        V::Money { .. } => "money",
        V::Price { .. } => "price",
        V::Qty { .. } => "qty",
        V::Ref { .. } => "ref",
        V::Rec { .. } => "record",
        V::Variant { .. } => "choice",
        V::List(_) | V::Set(_) => "list",
        V::Tuple(_) => "tuple",
        _ => "other",
    }
}

/// A value as a cell of the answer: `{v, d, kind}` (a record the user may not read shows no
/// name).
pub fn value_json(i: &mut Interp, v: &V) -> J {
    if let V::Ref { ent, id } = v {
        if !i.can("read", ent, None, Some(v)) {
            return json!({ "v": quadra_runtime::codec::to_json(v), "d": format!("{ent} #{id}"), "kind": "ref" });
        }
    }
    let mut j = quadra_api::json::cell(i, v);
    j["kind"] = json!(kind_name(v));
    j
}

fn out_json(i: &mut Interp, o: &Out) -> J {
    match o {
        Out::Ok(v) => value_json(i, &v.clone().normalized()),
        Out::Err { code, msg } => {
            json!({ "v": null, "d": code, "kind": "error", "code": code, "error": msg })
        }
    }
}

fn spill_json(i: &mut Interp, sp: &Spilled) -> J {
    let rows: Vec<J> = sp
        .rows
        .iter()
        .map(|r| {
            let mut cells: Vec<J> = r.iter().map(|v| if v.is_none() { J::Null } else { value_json(i, &v.clone().normalized()) }).collect();
            cells.resize(sp.width, J::Null);
            J::Array(cells)
        })
        .collect();
    let mut j = json!({ "rows": sp.rows.len(), "cols": sp.width, "cells": rows, "total": sp.total, "truncated": sp.total > sp.rows.len() });
    if let Some(h) = &sp.header {
        j["header"] = json!(h);
    }
    j
}

/// The evaluated workbook for an export: per sheet, (row, col) → value or error, spills
/// included.
pub fn grid_of(book: &Book, outs: &BTreeMap<Addr, Out>, spills: &HashMap<Addr, Spilled>) -> Vec<BTreeMap<(u32, u32), Out>> {
    let mut out: Vec<BTreeMap<(u32, u32), Out>> = vec![BTreeMap::new(); book.names.len()];
    for (a, o) in outs {
        if a.s < out.len() {
            out[a.s].insert((a.r, a.c), o.clone());
        }
    }
    for (a, sp) in spills {
        if a.s >= out.len() {
            continue;
        }
        for (dr, row) in sp.rows.iter().enumerate() {
            for (dc, v) in row.iter().enumerate() {
                let at = (a.r + dr as u32, a.c + dc as u32);
                out[a.s].insert(at, Out::Ok(v.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(s: &str) -> Result<(String, Vec<Ref>), (String, &'static str)> {
        let names = vec!["Sheet1".to_string(), "My sheet".to_string(), "Sales".to_string()];
        rewrite(s, 0, &Sheets { names: &names }, &|n| n == "Q1")
    }

    #[test]
    fn references_become_names() {
        let (t, r) = rw("A1 * 2 + $B$3").unwrap();
        assert_eq!(t, "__c_0_1_1 * 2 + __c_0_3_2");
        assert_eq!(r.len(), 2);
        let (t, _) = rw("B2:B20.sum() + 'My sheet'!A1:C3.count() + Sales!B2").unwrap();
        assert_eq!(t, "__r_0_2_2_20_2.sum() + __r_1_1_1_3_3.count() + __c_2_2_2");
        let (t, _) = rw("A:C.count() + A1#.count()").unwrap();
        assert_eq!(t, format!("__r_0_1_1_{MAX_ROWS}_3.count() + __sp_0_1_1.count()"));
        // strings, names, fields and calls are not references
        let (t, r) = rw("\"A1\" + o.B2 + Order2 + F(1) + Q1 + $Q$1").unwrap();
        assert_eq!(t, "\"A1\" + o.B2 + Order2 + F(1) + Q1 + __c_0_1_17");
        assert_eq!(r.len(), 1);
        let (t, _) = rw("x != A1").unwrap();
        assert_eq!(t, "x != __c_0_1_1");
        assert_eq!(rw("Nope!A1").unwrap_err().1, "#REF");
        assert_eq!(rw("A1 + #REF!").unwrap_err().1, "#REF");
        assert_eq!(rw("\"open").unwrap_err().1, "#ERROR");
        // a comment ends the formula
        assert_eq!(rw("A1 # note").unwrap().0, "__c_0_1_1 ");
    }

    #[test]
    fn addresses() {
        assert_eq!(parse_a1("A1"), Some((1, 1)));
        assert_eq!(parse_a1("AB12"), Some((12, 28)));
        assert_eq!(parse_a1("XFD1"), Some((1, 16384)));
        assert_eq!(parse_a1("XFE1"), None);
        assert_eq!(parse_a1("A0"), None);
        assert_eq!(col_name(28), "AB");
        assert_eq!(col_name(16384), "XFD");
    }
}
