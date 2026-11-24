//! Checking interface files with the model: every name a file uses (entities, fields, actions,
//! filters, selections, components) and every expression, with the type checker of the code.
//!
//! - **Names** (an entity, a field path, an action, a filter): looked up in the model directly,
//!   each found one becoming a reference of the file (go to definition, find references and
//!   rename in the editor go from `.qd` to `.qdi` and back).
//! - **Expressions** become a script of generated code, checked with the project as the
//!   formulas of an app are (engine/sheets.rs): per app, a function whose parameters are the
//!   app's filters and selections typed by what they filter (`@region` is `__p_region:
//!   [Region]`), whose `let`s are the app's, then one `let` per expression of a component; the
//!   rows' options of a view (`where`, `compute`, `color`) are formulas of its entity, as a saved
//!   view's are; a custom form's `submit` is a function of its fields; a field's `default` a
//!   function returning the field's type. Every piece of generated text copied from the file
//!   remembers where it came from: the checker's messages and references land on the file.
//!
//! Only the diagnostics and the references of the generated scripts are kept: the model
//! served is always the one of the `.qd` files alone.

use crate::ast::*;
use crate::Diag;
use quadra_ir::{EntityKind, Model, Ty};
use quadra_sema::{Analysis, Diagnostic, FileId, Project, Reference, Severity, Target as SemaTarget};
use quadra_syntax::{SyntaxKind as K, TextRange, TextSize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// The interface files of a project, checked.
pub struct Interfaces {
    /// the project with the generated scripts
    pub project: Project,
    /// its analysis, with the references of the interface files added (their ranges are in
    /// the interface files)
    pub analysis: Analysis,
    /// the problems of the interface files
    pub diagnostics: Vec<Diagnostic>,
    /// the trees of the interface files that parse
    pub trees: HashMap<FileId, File>,
    /// a generated script → its interface file
    pub generated: HashMap<FileId, FileId>,
    /// the parameters of the app of each interface file (`region`) with their types
    pub params: HashMap<FileId, Vec<(String, Option<String>)>>,
}

impl Interfaces {
    /// Is this file one of the generated scripts?
    pub fn is_generated(&self, f: FileId) -> bool {
        self.generated.contains_key(&f)
    }
}

/// Where generated scripts are put (scripts see every declaration; they are never served).
pub fn generated_path(root: &std::path::Path, rel: &str) -> PathBuf {
    root.join("tests").join("__qdi__").join(format!("{}.qd", rel.trim_end_matches(".qdi")))
}

// ---- generated text, with where it comes from -------------------------------------------------

#[derive(Default)]
struct Gen {
    text: String,
    /// (start in the text, length, offset in the file, identity: each byte maps to its own)
    segs: Vec<(u32, u32, u32, bool)>,
    /// (start in the text, the place in the file of what follows)
    ctxs: Vec<(u32, Span)>,
}

impl Gen {
    fn put(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn copy(&mut self, s: &str, file: u32) {
        self.segs.push((self.text.len() as u32, s.len() as u32, file, true));
        self.text.push_str(s);
    }

    fn point(&mut self, s: &str, file: u32) {
        self.segs.push((self.text.len() as u32, s.len() as u32, file, false));
        self.text.push_str(s);
    }

    fn ctx(&mut self, span: Span) {
        self.ctxs.push((self.text.len() as u32, span));
    }

    fn append(&mut self, o: Gen) {
        let shift = self.text.len() as u32;
        self.segs.extend(o.segs.into_iter().map(|(g, l, f, i)| (g + shift, l, f, i)));
        self.ctxs.extend(o.ctxs.into_iter().map(|(g, s)| (g + shift, s)));
        self.text.push_str(&o.text);
    }

    fn map(&self, g: u32) -> Option<u32> {
        let k = self.segs.partition_point(|s| s.0 <= g);
        let (start, len, file, identity) = *self.segs.get(k.checked_sub(1)?)?;
        if g < start + len {
            Some(if identity { file + (g - start) } else { file })
        } else {
            None
        }
    }

    /// A range of the text in the file: exactly when it is copied text, else the place of
    /// the expression or the line it belongs to.
    fn map_range(&self, r: TextRange) -> Option<Span> {
        let (a, b) = (u32::from(r.start()), u32::from(r.end()));
        if let (Some(x), Some(y)) = (self.map(a), self.map(b.saturating_sub(1).max(a))) {
            if y >= x {
                return Some(Span::new(x, y + 1));
            }
            return Some(Span::new(x, x + 1));
        }
        let k = self.ctxs.partition_point(|c| c.0 <= a);
        self.ctxs.get(k.checked_sub(1)?).map(|c| c.1)
    }
}

// ---- the model ---------------------------------------------------------------------------------

/// Fields every entity has without declaring them, with their types.
const IMPLICIT: &[(&str, &str)] = &[("id", "Int"), ("created_at", "DateTime"), ("updated_at", "DateTime"), ("version", "Int"), ("created_by", "User?"), ("updated_by", "User?")];

fn named(t: &Ty) -> Option<String> {
    match t {
        Ty::Path { segments, .. } => segments.last().cloned(),
        Ty::Optional(i) => named(i),
        _ => None,
    }
}

fn unopt(t: &Ty) -> &Ty {
    match t {
        Ty::Optional(i) => i,
        t => t,
    }
}

/// How the language writes a type, when a parameter can be declared with it.
fn type_text(t: &Ty) -> Option<String> {
    let t = unopt(t);
    if quadra_ir::inline_enum(t).is_some() || matches!(t, Ty::List(_) | Ty::Set(_) | Ty::Tuple(_) | Ty::Union(_) | Ty::Error) {
        return None;
    }
    Some(quadra_ir::print::ty(t))
}

/// The steps of a field path from an entity: (the entity a segment is of, the segment), and
/// the type at its end (none for an implicit field); or the index of the segment that fails
/// and why.
#[allow(clippy::type_complexity)]
fn walk(model: &Model, entity: &str, path: &str) -> Result<(Vec<(String, String)>, Option<Ty>), (usize, String)> {
    let segs: Vec<&str> = path.split('.').collect();
    let mut ent = entity.to_string();
    let mut steps = Vec::new();
    let mut last = None;
    for (k, seg) in segs.iter().enumerate() {
        let e = model.entity(&ent).ok_or((k, format!("`{ent}` is not an entity")))?;
        let ty = match model.find_field(e, seg) {
            Some(f) => f.ty.clone(),
            None => match IMPLICIT.iter().find(|(n, _)| n == seg) {
                Some(_) => None,
                None => {
                    let hint = closest(seg, e.fields.iter().map(|f| f.name.as_str()));
                    return Err((k, format!("`{ent}` has no field `{seg}`{}", hint.map(|h| format!(": did you mean `{h}`?")).unwrap_or_default())));
                }
            },
        };
        steps.push((ent.clone(), seg.to_string()));
        if k + 1 < segs.len() {
            let target = ty
                .as_ref()
                .and_then(named)
                .filter(|n| model.entity(n).is_some_and(|x| x.kind == EntityKind::Entity))
                .ok_or((k, format!("`{ent}.{seg}` is not a reference: `{}` cannot follow it", segs[k + 1])))?;
            ent = target;
        }
        last = ty;
    }
    Ok((steps, last))
}

/// The closest name (a likely typo), if one is close enough.
fn closest<'a>(name: &str, options: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for o in options {
        let d = distance(name, o);
        // a renamed name often keeps the old one: `total` → `amount_total`
        let contains = name.len() >= 3 && o != name && (o.contains(name) || name.contains(o)) && o.len().min(name.len()) >= 3;
        let d = if contains { d.min(2) } else { d };
        if d <= 2.max(name.len() / 3) && best.map_or(true, |b| d < b.0) {
            best = Some((d, o));
        }
    }
    best.map(|b| b.1.to_string())
}

fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

// ---- checking a file ---------------------------------------------------------------------------

/// A reference to resolve once the project with the generated scripts is analysed.
enum Pending {
    Entity(String),
    Member(String, String),
}

struct Ctx<'a> {
    model: &'a Model,
    src: &'a str,
    diags: Vec<Diag>,
    refs: Vec<(Span, Pending)>,
    /// the app's parameters: name → (variable, type)
    params: Vec<(String, String, Option<String>)>,
    known: HashSet<String>,
    /// the parameters as a function declares them (`__p_region: [Region], …`)
    params_text: String,
    unknown: Vec<(String, Span)>,
    /// sheet components of the app
    sheets: HashSet<String>,
    /// date-range filters
    ranges: HashSet<String>,
    filters: HashSet<String>,
    counter: usize,
}

impl<'a> Ctx<'a> {
    fn err(&mut self, msg: impl Into<String>, span: Span) {
        self.diags.push(Diag::new(msg, span));
    }

    fn entity(&mut self, name: &str, span: Span) -> bool {
        match self.model.entity(name) {
            Some(e) if e.kind == EntityKind::Entity => {
                self.refs.push((span, Pending::Entity(name.to_string())));
                true
            }
            Some(_) => {
                self.refs.push((span, Pending::Entity(name.to_string())));
                self.err(format!("`{name}` is not an entity (a record or a trait has no rows)"), span);
                false
            }
            None => {
                let hint = closest(name, self.model.entities.iter().filter(|e| e.kind == EntityKind::Entity).map(|e| e.name.as_str()));
                self.err(format!("unknown entity `{name}`{}", hint.map(|h| format!(": did you mean `{h}`?")).unwrap_or_default()), span);
                false
            }
        }
    }

    /// A field path of `entity` written at `span` (the references of its segments).
    fn path(&mut self, entity: &str, path: &str, span: Span) -> Option<Option<Ty>> {
        let exact = (span.end - span.start) as usize == path.len() && span.end > 0;
        let seg_span = |k: usize| -> Span {
            if !exact {
                return span;
            }
            let start: usize = path.split('.').take(k).map(|s| s.len() + 1).sum();
            let len = path.split('.').nth(k).map_or(0, str::len);
            Span::new(span.start + start as u32, span.start + (start + len) as u32)
        };
        match walk(self.model, entity, path) {
            Ok((steps, ty)) => {
                for (k, (ent, seg)) in steps.into_iter().enumerate() {
                    if IMPLICIT.iter().any(|(n, _)| *n == seg) && self.model.entity(&ent).and_then(|e| self.model.find_field(e, &seg)).is_none() {
                        continue;
                    }
                    self.refs.push((seg_span(k), Pending::Member(ent, seg)));
                }
                Some(ty)
            }
            Err((k, msg)) => {
                // the segments before the one that fails are references
                if let Ok((steps, _)) = walk(self.model, entity, &path.split('.').take(k).collect::<Vec<_>>().join(".")) {
                    for (j, (ent, seg)) in steps.into_iter().enumerate() {
                        self.refs.push((seg_span(j), Pending::Member(ent, seg)));
                    }
                }
                self.err(msg, seg_span(k));
                None
            }
        }
    }

    fn param(&mut self, name: &str, span: Span) {
        if !self.known.contains(name) {
            self.unknown.push((name.to_string(), span));
        }
    }

    /// Copy an expression into generated text; `@name` becomes the app's parameter.
    fn expr(&mut self, g: &mut Gen, e: &Expr) {
        g.ctx(e.span);
        if e.raw {
            g.put("none");
            self.err("not an expression (written `raw \"…\"`): write it as an expression", e.span);
            return;
        }
        let (toks, _) = quadra_syntax::lexer::tokenize(&e.text);
        let toks: Vec<_> = toks.into_iter().filter(|t| !matches!(t.kind, K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT)).collect();
        if toks.len() != e.map.len() {
            g.copy(&e.text, e.span.start);
            return;
        }
        let mut prev_end = 0usize;
        for (k, t) in toks.iter().enumerate() {
            let (a, b) = (usize::from(t.range.start()), usize::from(t.range.end()));
            g.put(&e.text[prev_end..a]);
            let file = e.map[k].1;
            if t.kind == K::AT {
                g.point("__p_", file);
                if let Some(n) = toks.get(k + 1) {
                    let name = &e.text[n.range];
                    let span = Span::new(file, e.map[k + 1].1 + name.len() as u32);
                    self.param(name, span);
                }
            } else {
                g.copy(&e.text[a..b], file);
            }
            prev_end = b;
        }
    }

    fn next(&mut self) -> usize {
        self.counter += 1;
        self.counter
    }
}

/// The span of the first name `text` among those written on a line (not yet taken).
fn name_span(names: &[Name], text: &str, taken: &mut HashSet<usize>) -> Span {
    for (k, n) in names.iter().enumerate() {
        if n.text == text && !taken.contains(&k) {
            taken.insert(k);
            return n.span;
        }
    }
    Span::default()
}

fn opt<'l>(lines: &'l [Line], key: &str) -> Option<(&'l Value, &'l [Name], Span)> {
    lines.iter().find_map(|l| match l {
        Line::Opt { key: k, value, names, span } if k.text == key => Some((value, names.as_slice(), *span)),
        _ => None,
    })
}

fn word<'l>(lines: &'l [Line], key: &str) -> Option<(&'l str, Span)> {
    let (v, names, span) = opt(lines, key)?;
    match v {
        Value::Word(w) => Some((w.as_str(), names.iter().find(|n| &n.text == w).map_or(span, |n| n.span))),
        _ => None,
    }
}

fn visit<'a>(blocks: &'a [Block], f: &mut impl FnMut(&'a Component)) {
    for b in blocks {
        match b {
            Block::Component(c) => f(c),
            Block::Row(r) => visit(&r.children, f),
            Block::Column(c) => visit(&c.children, f),
            Block::Space(..) => {}
        }
    }
}

fn is_addr(w: &str) -> bool {
    let letters = w.chars().take_while(|c| c.is_ascii_uppercase()).count();
    (1..=3).contains(&letters) && w.len() > letters && w[letters..].chars().all(|c| c.is_ascii_digit()) && !w[letters..].starts_with('0')
}

/// Does a formula refer to cells (checked by the spreadsheet, not here)?
fn has_cell_refs(f: &str) -> bool {
    if f.contains(['!', '$', '\'', '#']) {
        return true;
    }
    let (toks, _) = quadra_syntax::lexer::tokenize(f);
    let sig: Vec<_> = toks.iter().filter(|t| !matches!(t.kind, K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT)).collect();
    let column = |w: &str| !w.is_empty() && w.len() <= 3 && w.chars().all(|c| c.is_ascii_uppercase());
    sig.iter().enumerate().any(|(k, t)| {
        let w = &f[t.range];
        (t.kind == K::IDENT && is_addr(w))
            || (t.kind == K::COLON && k > 0 && sig.get(k + 1).is_some_and(|n| {
                let (a, b) = (&f[sig[k - 1].range], &f[n.range]);
                (is_addr(a) || column(a)) && (is_addr(b) || column(b))
            }))
    })
}

/// A file-level expression of a formula written at `span` (a cell, a Markdown `{= … }`).
fn plain_expr(text: &str, span: Span) -> Expr {
    let (toks, _) = quadra_syntax::lexer::tokenize(text);
    let map = toks.iter().filter(|t| !matches!(t.kind, K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT)).map(|t| (u32::from(t.range.start()), span.start + u32::from(t.range.start()))).collect();
    Expr { text: text.to_string(), raw: false, span, map }
}

/// The app's parameters: (name, variable, type).
fn app_params(model: &Model, app: &App, ctx_diags: &mut Vec<Diag>, refs: &mut Vec<(Span, Pending)>, ranges: &mut HashSet<String>) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    for f in &app.filters {
        let entity = &f.entity.text;
        let ty = match model.entity(entity) {
            Some(e) if e.kind == EntityKind::Entity => {
                refs.push((f.entity.span, Pending::Entity(entity.clone())));
                let mut tmp = Ctx { model, src: "", diags: Vec::new(), refs: Vec::new(), params: Vec::new(), known: HashSet::new(), params_text: String::new(), unknown: Vec::new(), sheets: HashSet::new(), ranges: HashSet::new(), filters: HashSet::new(), counter: 0 };
                let t = tmp.path(entity, &f.field.text, f.field.span);
                ctx_diags.extend(tmp.diags);
                refs.extend(tmp.refs);
                t
            }
            _ => {
                ctx_diags.push(Diag::new(format!("unknown entity `{entity}`"), f.entity.span));
                None
            }
        };
        let name = &f.name.text;
        let var = format!("__p_{name}");
        let base = ty.clone().flatten();
        let tt = base.as_ref().and_then(type_text);
        let kind = base.as_ref().map(|t| named(unopt(t)).unwrap_or_default()).unwrap_or_default();
        let found = ty.is_some();
        let bad = |what: &str| Diag::new(format!("filter `{name}`: {what}"), f.widget.span);
        match f.widget.text.as_str() {
            "select" => out.push((name.clone(), var, tt.map(|t| format!("{t}?")))),
            "multi" => out.push((name.clone(), var, tt.map(|t| format!("[{t}]")))),
            "date_range" => {
                if found && base.is_some() && !matches!(kind.as_str(), "Date" | "DateTime") {
                    ctx_diags.push(bad("a date range filters a date"));
                }
                ranges.insert(name.clone());
                out.push((name.clone(), var.clone(), Some("DateRange".into())));
                out.push((format!("{name}__on"), format!("{var}__on"), Some("Bool".into())));
            }
            "number_range" => {
                if found && base.is_some() && !matches!(kind.as_str(), "Int" | "Decimal" | "Money" | "Qty" | "Percent" | "Price") {
                    ctx_diags.push(bad("a number range filters a number or an amount"));
                }
                out.push((format!("{name}_min"), format!("{var}_min"), tt.clone().map(|t| format!("{t}?"))));
                out.push((format!("{name}_max"), format!("{var}_max"), tt.map(|t| format!("{t}?"))));
            }
            "toggle" => {
                if found && base.is_some() && kind != "Bool" {
                    ctx_diags.push(bad("a toggle filters a yes/no field"));
                }
                out.push((name.clone(), var, Some("Bool".into())));
            }
            _ => out.push((name.clone(), var, Some("Text".into()))),
        }
    }
    // the records selected in views and published by custom forms
    let mut seen: HashSet<String> = out.iter().map(|p| p.0.clone()).collect();
    for p in &app.pages {
        visit(&p.body, &mut |c| {
            let sel = match c.kind.as_str() {
                "view" => match (word(&c.lines, "select"), word(&c.lines, "of")) {
                    (Some((s, _)), Some((e, _))) => Some((s.to_string(), e.to_string())),
                    _ => None,
                },
                "form" => c.lines.iter().find_map(|l| match l {
                    Line::Opt { value: Value::After(After::Select { entity, name }), .. } => Some((name.clone(), entity.clone())),
                    _ => None,
                }),
                _ => None,
            };
            if let Some((s, e)) = sel {
                if seen.insert(s.clone()) {
                    let ty = model.entity(&e).map(|_| format!("{e}?"));
                    out.push((s.clone(), format!("__p_{s}"), ty));
                }
            }
        });
    }
    out
}

/// The generated script of an interface file, and what it found.
fn generate(model: &Model, file: &File, src: &str) -> (Gen, Vec<Diag>, Vec<(Span, Pending)>, Vec<(String, Option<String>)>) {
    let mut out = Gen::default();
    let mut diags = Vec::new();
    let mut refs = Vec::new();
    let mut all_params = Vec::new();
    let mut counter = 0;
    for item in &file.items {
        match item {
            Item::App(app) => {
                let mut ranges = HashSet::new();
                let mut params = app_params(model, app, &mut diags, &mut refs, &mut ranges);
                // the `@names` the file writes that are not parameters: reported where they are,
                // declared without a type so that nothing else fails because of them
                let known: HashSet<String> = params.iter().map(|p| p.0.clone()).collect();
                let region = &src[(app.span.start as usize).min(src.len())..(app.span.end as usize).min(src.len())];
                let (toks, _) = quadra_syntax::lexer::tokenize(region);
                let sig: Vec<_> = toks.iter().filter(|t| !matches!(t.kind, K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT)).collect();
                let mut extra_names: Vec<String> = Vec::new();
                for (k, t) in sig.iter().enumerate() {
                    if t.kind == K::AT {
                        if let Some(n) = sig.get(k + 1).filter(|n| n.kind == K::IDENT && n.range.start() == t.range.end()) {
                            let name = region[n.range].to_string();
                            if !known.contains(&name) && !extra_names.contains(&name) {
                                extra_names.push(name);
                            }
                        }
                    }
                }
                let declared: Vec<(String, String, Option<String>)> = params.iter().cloned().chain(extra_names.iter().map(|n| (n.clone(), format!("__p_{n}"), None))).collect();
                let params_text: String = declared
                    .iter()
                    .map(|(_, v, t)| match t {
                        Some(t) => format!("{v}: {t}, "),
                        None => format!("{v}, "),
                    })
                    .collect();
                let mut ctx = Ctx {
                    model,
                    src,
                    diags: Vec::new(),
                    refs: Vec::new(),
                    known,
                    params: std::mem::take(&mut params),
                    params_text,
                    unknown: Vec::new(),
                    sheets: HashSet::new(),
                    ranges,
                    filters: app.filters.iter().map(|f| f.name.text.clone()).collect(),
                    counter,
                };
                for p in &app.pages {
                    visit(&p.body, &mut |c| {
                        if c.kind == "sheet" {
                            if let Some(n) = &c.name {
                                ctx.sheets.insert(n.text.clone());
                            }
                        }
                    });
                }
                let mut body = Gen::default();
                let mut extra = Gen::default();
                for l in &app.lets {
                    body.ctx(l.span);
                    body.put("    let ");
                    body.copy(&l.name.text, l.name.span.start);
                    body.put(" = ");
                    ctx.expr(&mut body, &l.expr);
                    body.put("\n");
                }
                for p in &app.pages {
                    let mut comps = Vec::new();
                    visit(&p.body, &mut |c| comps.push(c));
                    for c in comps {
                        component(&mut ctx, c, &mut body, &mut extra);
                    }
                }
                for (name, span) in std::mem::take(&mut ctx.unknown) {
                    let known: Vec<String> = ctx.params.iter().filter(|p| !p.0.ends_with("__on")).map(|p| format!("@{}", p.0)).collect();
                    ctx.err(
                        if known.is_empty() {
                            format!("`@{name}`: this app has no filter or selected record of that name")
                        } else {
                            format!("`@{name}`: no filter or selected record of that name (there are {})", known.join(", "))
                        },
                        span,
                    );
                }
                let mut head = Gen::default();
                head.ctx(app.span);
                let n = ctx.next();
                head.put(&format!("fn __qdi_app_{n}({}):\n", ctx.params_text.trim_end_matches([',', ' '])));
                body.put("    return none\n");
                head.append(body);
                head.append(extra);
                out.append(head);
                diags.extend(std::mem::take(&mut ctx.diags));
                refs.extend(std::mem::take(&mut ctx.refs));
                all_params = ctx.params.iter().map(|p| (p.0.clone(), p.2.clone())).collect();
                counter = ctx.counter;
            }
            Item::View(v) => {
                let mut ctx = Ctx { model, src, diags: Vec::new(), refs: Vec::new(), params: Vec::new(), known: HashSet::new(), params_text: String::new(), unknown: Vec::new(), sheets: HashSet::new(), ranges: HashSet::new(), filters: HashSet::new(), counter };
                let mut g = Gen::default();
                if ctx.entity(&v.entity.text, v.entity.span) {
                    view_rows(&mut ctx, &v.entity.text, &v.lines, &mut g);
                }
                for (name, span) in std::mem::take(&mut ctx.unknown) {
                    ctx.err(format!("`@{name}`: a saved view has no filters or selections"), span);
                }
                out.append(g);
                diags.extend(ctx.diags);
                refs.extend(ctx.refs);
                counter = ctx.counter;
            }
        }
    }
    (out, diags, refs, all_params)
}

/// The span of the name `text` written on a line, else the line's.
fn span_of(names: &[Name], text: &str, line: Span) -> Span {
    names.iter().find(|n| n.text == text).map_or(line, |n| n.span)
}

/// A `let` of the app's function for an expression.
fn let_expr(ctx: &mut Ctx, g: &mut Gen, e: &Expr) {
    let n = ctx.next();
    g.put(&format!("    let __qdi_{n} = "));
    ctx.expr(g, e);
    g.put("\n");
}

fn source_params(ctx: &mut Ctx, s: &Source, names: &[Name], line: Span) {
    if let Source::Param(p) = s {
        let span = span_of(names, &format!("@{p}"), line);
        ctx.param(p, span);
    }
}

fn component(ctx: &mut Ctx, c: &Component, body: &mut Gen, extra: &mut Gen) {
    let kind = c.kind.as_str();
    // the entity of a view, a form
    let entity: Option<String> = match kind {
        "view" => word(&c.lines, "of").and_then(|(e, span)| ctx.entity(e, span).then(|| e.to_string())),
        "form" => word(&c.lines, "entity").and_then(|(e, span)| ctx.entity(e, span).then(|| e.to_string())),
        _ => None,
    };
    let mut fields: Vec<&FormField> = Vec::new();
    for l in &c.lines {
        match l {
            Line::Opt { key, value, names, span } => {
                let mut taken = HashSet::new();
                match (kind, key.text.as_str(), value) {
                    (_, _, Value::Source(DataSource::Expr(e))) | (_, "sparkline" | "message", Value::Expr(e)) | ("kpi", "compare", Value::Compare(Compare::Target(e))) => {
                        if !(kind == "form" && key.text == "message") {
                            let_expr(ctx, body, e);
                        }
                    }
                    (_, _, Value::Source(DataSource::Cell { component, .. } | DataSource::Range { component, .. })) => {
                        if !ctx.sheets.contains(component) {
                            let span = name_span(names, component, &mut taken);
                            ctx.err(format!("no spreadsheet `{component}` in the app"), if span == Span::default() { c.span } else { span });
                        }
                    }
                    ("kpi", "compare", Value::Compare(Compare::Previous(f))) => {
                        let s = span_of(names, &format!("@{f}"), *span);
                        if !ctx.ranges.contains(f) {
                            ctx.err(format!("`compare: previous @{f}`: `@{f}` is not a date range filter of the app"), s);
                        }
                    }
                    ("filter", "show", Value::Names(list)) => {
                        for n in list {
                            let s = name_span(names, n, &mut taken);
                            if !ctx.filters.contains(n) {
                                let hint = closest(n, ctx.filters.iter().map(String::as_str));
                                ctx.err(format!("no filter `{n}` in the app (declare it: `filter {n}: …`){}", hint.map(|h| format!(": did you mean `{h}`?")).unwrap_or_default()), if s == Span::default() { *span } else { s });
                            }
                        }
                    }
                    ("button", "run", Value::Run(run)) => run_check(ctx, run, names, *span),
                    ("form", "fields", Value::Names(list)) => {
                        if let Some(e) = &entity {
                            for n in list {
                                let s = name_span(names, n, &mut taken);
                                ctx.path(e, n, s);
                            }
                        }
                    }
                    (_, _, Value::From(s)) => source_params(ctx, s, names, *span),
                    _ => {}
                }
            }
            Line::Section { fields: list, names, span, .. } => {
                if let Some(e) = &entity {
                    let mut taken = HashSet::new();
                    for n in list {
                        let s = name_span(names, n, &mut taken);
                        ctx.path(e, n, if s == Span::default() { *span } else { s });
                    }
                }
            }
            Line::Default { field, source, span } => {
                if let Some(e) = &entity {
                    ctx.path(e, &field.text, field.span);
                }
                if let Source::Param(p) = source {
                    ctx.param(p, *span);
                }
            }
            Line::Apply { entity: ae, binds, .. } => {
                if ctx.entity(&ae.text, ae.span) {
                    for (f, p) in binds {
                        if !ctx.filters.contains(&f.text) && !ctx.known.contains(&f.text) {
                            ctx.err(format!("no filter `{}` in the app", f.text), f.span);
                        }
                        if let Some(p) = p {
                            ctx.path(&ae.text, &p.text, p.span);
                        }
                    }
                }
            }
            Line::Text(t, span) if kind == "markdown" => markdown(ctx, t, *span, body),
            Line::Tab(tab) => {
                for tl in &tab.lines {
                    if let TabLine::Cell { value: Some(CellValue::Formula(f, fspan)), .. } = tl {
                        if !has_cell_refs(f) {
                            // where the formula's text starts in the file
                            let at = ctx.src.get(fspan.start as usize..fspan.end as usize).and_then(|s| s.find(f.as_str())).map_or(fspan.start, |k| fspan.start + k as u32);
                            let e = plain_expr(f, Span::new(at, at + f.len() as u32));
                            let_expr(ctx, body, &e);
                        }
                    }
                }
            }
            Line::Field(f) => fields.push(f),
            _ => {}
        }
    }
    if kind == "view" {
        if let Some(e) = &entity {
            view_rows(ctx, e, &c.lines, extra);
        }
    }
    if kind == "form" && (!fields.is_empty() || c.lines.iter().any(|l| matches!(l, Line::Submit { .. }))) {
        custom_form(ctx, c, &fields, extra);
    }
}

/// The `{= … }` of a Markdown text.
fn markdown(ctx: &mut Ctx, t: &str, span: Span, body: &mut Gen) {
    let raw = ctx.src.get(span.start as usize..span.end as usize).unwrap_or("");
    let mut rest = t;
    let mut search_from = 0usize;
    while let Some(k) = rest.find("{=") {
        let after = &rest[k + 2..];
        let Some(end) = after.find('}') else { break };
        let e = after[..end].trim();
        rest = &after[end + 1..];
        if e.is_empty() || has_cell_refs(e) {
            continue;
        }
        let at = raw[search_from.min(raw.len())..].find(e).map(|x| x + search_from);
        let file = at.map_or(span.start, |x| span.start + x as u32);
        if let Some(x) = at {
            search_from = x + e.len();
        }
        let expr = plain_expr(e, Span::new(file, file + e.len() as u32));
        let_expr(ctx, body, &expr);
    }
}

/// A button's action: its entity, the action (or the flow transition), its parameters.
fn run_check(ctx: &mut Ctx, run: &Run, names: &[Name], line: Span) {
    let entity = match &run.target {
        Target::Param(p) => {
            let s = span_of(names, &format!("@{p}"), line);
            ctx.param(p, s);
            let ty = ctx.params.iter().find(|x| &x.0 == p).and_then(|x| x.2.clone());
            match ty.as_deref().map(|t| t.trim_end_matches('?')) {
                Some(e) if ctx.model.entity(e).is_some() => Some(e.to_string()),
                _ => {
                    if ctx.known.contains(p) {
                        ctx.err(format!("`@{p}` is not a selected record: write `<Entity>[@{p}].{}(…)`", run.action), s);
                    }
                    None
                }
            }
        }
        Target::Entity(e) | Target::Of(e, _) => {
            let s = span_of(names, e, line);
            if let Target::Of(_, src) = &run.target {
                source_params(ctx, src, names, line);
            }
            ctx.entity(e, s).then(|| e.clone())
        }
    };
    let Some(entity) = entity else { return };
    let e = ctx.model.entity(&entity).unwrap();
    let action_span = span_of(names, &run.action, line);
    let action = e.actions.iter().find(|a| a.name == run.action);
    let transition = e.flows.iter().any(|f| f.transitions.iter().any(|t| t.by.as_deref() == Some(run.action.as_str())));
    if action.is_none() && !transition {
        let options: Vec<&str> = e.actions.iter().map(|a| a.name.as_str()).chain(e.flows.iter().flat_map(|f| f.transitions.iter().filter_map(|t| t.by.as_deref()))).collect();
        let hint = closest(&run.action, options.iter().copied());
        ctx.err(format!("`{entity}` has no action `{}`{}", run.action, hint.map(|h| format!(": did you mean `{h}`?")).unwrap_or_default()), action_span);
        return;
    }
    ctx.refs.push((action_span, Pending::Member(entity.clone(), run.action.clone())));
    for (arg, src) in &run.args {
        let s = span_of(names, arg, line);
        let ok = action.is_some_and(|a| a.params.iter().any(|p| &p.name == arg));
        if !ok {
            let list: Vec<String> = action.map(|a| a.params.iter().map(|p| p.name.clone()).collect()).unwrap_or_default();
            ctx.err(
                if list.is_empty() {
                    format!("`{}` takes no parameters", run.action)
                } else {
                    format!("`{}` has no parameter `{arg}` (it has {})", run.action, list.join(", "))
                },
                s,
            );
        }
        source_params(ctx, src, names, line);
    }
}

/// The options of a view's rows: the paths of its entity, its expressions as formulas of it.
fn view_rows(ctx: &mut Ctx, entity: &str, lines: &[Line], extra: &mut Gen) {
    let personal: HashSet<&str> = lines.iter().filter_map(|l| if let Line::Compute { name, .. } = l { Some(name.text.as_str()) } else { None }).collect();
    let mut formulas = Gen::default();
    for l in lines {
        match l {
            Line::Opt { key, value, names, span } => {
                let mut taken = HashSet::new();
                let mut path = |ctx: &mut Ctx, p: &str| {
                    if personal.contains(p) {
                        return;
                    }
                    let s = name_span(names, p, &mut taken);
                    ctx.path(entity, p, if s == Span::default() { *span } else { s });
                };
                match (key.text.as_str(), value) {
                    ("fields" | "cards", Value::Names(list)) => list.iter().for_each(|p| path(ctx, p)),
                    ("widths" | "aggregates", Value::Pairs(ps)) => ps.iter().for_each(|(p, _)| path(ctx, p)),
                    ("sort", Value::Sort(s)) => s.iter().for_each(|(p, _)| path(ctx, p)),
                    ("group", Value::Group(g)) => g.iter().for_each(|(p, _)| path(ctx, p)),
                    ("by" | "card_title" | "sum", Value::Word(w)) => path(ctx, w),
                    ("where", Value::Expr(e)) => formula(ctx, &mut formulas, e),
                    _ => {}
                }
            }
            Line::Compute { expr, .. } | Line::Color { expr, .. } => formula(ctx, &mut formulas, expr),
            _ => {}
        }
    }
    if !formulas.text.is_empty() {
        extra.put(&format!("extend {entity}:\n"));
        extra.append(formulas);
    }
}

fn formula(ctx: &mut Ctx, g: &mut Gen, e: &Expr) {
    let n = ctx.next();
    g.put(&format!("    __qdi_{n} = "));
    ctx.expr(g, e);
    g.put("\n");
}

/// A custom form: its submit as a function of its fields (and of the app's parameters), each
/// field's `default` as a function returning the field's type.
fn custom_form(ctx: &mut Ctx, c: &Component, fields: &[&FormField], extra: &mut Gen) {
    let n = ctx.next();
    let mut g = Gen::default();
    g.ctx(c.span);
    g.put(&format!("fn __qdi_form_{n}({}", ctx.params_text));
    for f in fields {
        g.copy(&f.name.text, f.name.span.start);
        g.put(": ");
        g.copy(&f.ty.text, f.ty.span.start);
        g.put(", ");
    }
    g.put("):\n");
    for f in fields {
        if let Some(w) = &f.where_ {
            let t = w.text.trim_start();
            let op = ["==", "!=", "<=", ">=", "<", ">"].iter().any(|o| t.starts_with(o));
            if op || t.contains("..") {
                let k = ctx.next();
                g.ctx(w.span);
                g.put(&format!("    let __qdi_{k} = "));
                g.copy(&f.name.text, f.name.span.start);
                g.put(if op { " " } else { " in " });
                let e = Expr { text: w.text.clone(), raw: false, span: w.span, map: w.map.clone() };
                ctx.expr(&mut g, &e);
                g.put("\n");
            }
        }
    }
    if let Some((Value::Expr(m), _, _)) = opt(&c.lines, "message") {
        let k = ctx.next();
        g.put(&format!("    let __qdi_{k} = "));
        ctx.expr(&mut g, m);
        g.put("\n");
    }
    let submit = c.lines.iter().find_map(|l| if let Line::Submit { code_span, .. } = l { Some(*code_span) } else { None });
    match submit {
        Some(cs) => {
            let block = ctx.src.get(cs.start as usize..cs.end as usize).unwrap_or("");
            let indent = block.chars().take_while(|c| *c == ' ').count();
            let mut at = cs.start as usize;
            for line in block.split('\n') {
                let strip = line.chars().take(indent).take_while(|c| *c == ' ').count();
                let content = line[strip..].trim_end();
                if content.is_empty() {
                    g.put("\n");
                } else {
                    g.ctx(Span::new((at + strip) as u32, (at + strip + content.len()) as u32));
                    g.put("    ");
                    g.copy(content, (at + strip) as u32);
                    g.put("\n");
                }
                at += line.len() + 1;
            }
        }
        None => g.put("    return none\n"),
    }
    for f in fields {
        if let Some(d) = &f.default {
            let k = ctx.next();
            g.ctx(d.span);
            g.put(&format!("fn __qdi_{k}({}) -> ", ctx.params_text.trim_end_matches([',', ' '])));
            g.copy(&f.ty.text, f.ty.span.start);
            g.put(":\n    return ");
            ctx.expr(&mut g, d);
            g.put("\n");
        }
    }
    extra.append(g);
}

// ---- the whole project -------------------------------------------------------------------------

fn diagnostic(file: FileId, src: &str, d: &Diag, severity: Severity) -> Diagnostic {
    let len = src.len() as u32;
    let (a, b) = (d.span.start.min(len), d.span.end.min(len).max(d.span.start.min(len)));
    Diagnostic { file, range: TextRange::new(TextSize::from(a), TextSize::from(b)), severity, message: d.message.clone() }
}

/// A hint for the checker's `` `E` has no field `x` ``: the closest field of `E`.
fn with_hint(model: Option<&Model>, m: String) -> String {
    if m.contains("did you mean") {
        return m;
    }
    let Some(model) = model else { return m };
    let parts: Vec<&str> = m.split('`').collect();
    if parts.len() >= 4 && parts[2] == " has no field " {
        if let Some(e) = model.entity(parts[1]) {
            let options = e.fields.iter().map(|f| f.name.as_str()).chain(e.actions.iter().map(|a| a.name.as_str()));
            if let Some(h) = closest(parts[3], options) {
                return format!("{m}: did you mean `{h}`?");
            }
        }
    }
    m
}

/// The `rename E.old -> E.new` of the project's migrations: (entity, old, new).
pub fn migration_renames(project: &Project) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for f in project.user_files() {
        let rel = f.path.strip_prefix(&project.root).unwrap_or(&f.path);
        if !rel.starts_with("migrations") {
            continue;
        }
        for line in f.text.lines() {
            let Some(rest) = line.trim_start().strip_prefix("rename ") else { continue };
            let rest = rest.split('#').next().unwrap_or("");
            let Some((a, b)) = rest.split_once("->").or_else(|| rest.split_once('→')) else { continue };
            let (a, b) = (a.trim(), b.trim());
            if let (Some((e, old)), Some(new)) = (a.rsplit_once('.').map(|(e, o)| (e.rsplit('.').next().unwrap_or(e), o)), b.rsplit('.').next()) {
                out.push((e.to_string(), old.to_string(), new.to_string()));
            }
        }
    }
    out
}

/// `` `E` has no field `x` `` when a migration renamed `x`: the new name, as the fix.
fn rename_hint(renames: &[(String, String, String)], m: &str) -> Option<String> {
    let parts: Vec<&str> = m.split('`').collect();
    if parts.len() >= 4 && parts[2] == " has no field " {
        let (e, x) = (parts[1], parts[3]);
        let (_, _, new) = renames.iter().rev().find(|(re, old, _)| re == e && old == x)?;
        return Some(format!("`{e}` has no field `{x}`: a migration renamed it `{new}`: did you mean `{new}`?"));
    }
    None
}

/// The checker's message about generated code, in the words of the file.
fn message(m: &str) -> String {
    let mut out = m.replace("__p_", "@");
    // `__qdi_12` names a generated `let`: the expression itself
    while let Some(k) = out.find("`__qdi_") {
        let rest = &out[k + 1..];
        let end = rest.find('`').map_or(rest.len(), |e| e);
        out = format!("{}this expression{}", &out[..k], &rest[end + 1.min(rest.len() - end)..]);
    }
    out
}

/// Check the interface files of a project against its model (`None`: the code has errors,
/// only the files' own problems are reported).
pub fn check(project: &Project, model: Option<&Model>) -> Interfaces {
    let mut diagnostics = Vec::new();
    let mut trees = HashMap::new();
    let mut gens: Vec<(FileId, PathBuf, Gen)> = Vec::new();
    let mut pending: Vec<(FileId, Span, Pending)> = Vec::new();
    let mut params = HashMap::new();
    for f in project.interface_files() {
        let (tree, diags) = crate::parse(&f.text);
        let parsed = diags.is_empty();
        for d in &diags {
            diagnostics.push(diagnostic(f.id, &f.text, d, Severity::Error));
        }
        if parsed {
            if let Err(ds) = crate::lower(&tree) {
                for d in &ds {
                    diagnostics.push(diagnostic(f.id, &f.text, d, Severity::Error));
                }
            }
        }
        if let Some(model) = model {
            let (g, ds, refs, ps) = generate(model, &tree, &f.text);
            for d in &ds {
                diagnostics.push(diagnostic(f.id, &f.text, d, Severity::Error));
            }
            pending.extend(refs.into_iter().map(|(s, p)| (f.id, s, p)));
            params.insert(f.id, ps);
            let rel = f.path.strip_prefix(&project.root).unwrap_or(&f.path).to_string_lossy().replace('\\', "/");
            let rel = rel.strip_prefix("interfaces/").unwrap_or(&rel).to_string();
            if !g.text.is_empty() {
                gens.push((f.id, generated_path(&project.root, &rel), g));
            }
        }
        trees.insert(f.id, tree);
    }
    let scripts: Vec<(PathBuf, String)> = gens.iter().map(|(_, p, g)| (p.clone(), format!("{}\n", g.text))).collect();
    if std::env::var_os("QUADRA_DEBUG_QDI").is_some() {
        for (p, t) in &scripts {
            eprintln!("--- {}\n{t}", p.display());
        }
    }
    let extended = project.with_scripts(scripts);
    let mut analysis = extended.analyze();
    let mut generated = HashMap::new();
    let mut by_gen: HashMap<FileId, (FileId, &Gen)> = HashMap::new();
    for (qdi, path, g) in &gens {
        if let Some(f) = extended.file_by_path(path) {
            generated.insert(f.id, *qdi);
            by_gen.insert(f.id, (*qdi, g));
        }
    }
    let map_diag = |d: &Diagnostic, out: &mut Vec<Diagnostic>| {
        if let Some((qdi, g)) = by_gen.get(&d.file) {
            if let Some(span) = g.map_range(d.range) {
                let src = &extended.file(*qdi).text;
                let msg = with_hint(model, message(&d.message));
                let dd = diagnostic(*qdi, src, &Diag::new(msg, span), d.severity);
                out.push(dd);
            }
        }
    };
    let name_errors: Vec<Diagnostic> = analysis.diagnostics.iter().filter(|d| by_gen.contains_key(&d.file)).cloned().collect();
    for d in &name_errors {
        if d.severity == Severity::Error {
            map_diag(d, &mut diagnostics);
        }
    }
    // the types, when every name is known
    let code_errors = analysis.diagnostics.iter().any(|d| d.severity == Severity::Error);
    let mut accesses = Vec::new();
    if model.is_some() && !code_errors {
        let m = quadra_ir::lower(&extended, &analysis);
        let report = quadra_types::analyze_model(&m);
        for d in report.diagnostics.iter().filter(|d| by_gen.contains_key(&d.file) && d.severity != Severity::Hint) {
            map_diag(d, &mut diagnostics);
        }
        accesses = report.accesses;
    }
    // the references: those of the generated code, where they were copied from; the names
    let mut refs: Vec<Reference> = Vec::new();
    for r in &analysis.references {
        if let Some((qdi, g)) = by_gen.get(&r.file) {
            let (a, b) = (u32::from(r.range.start()), u32::from(r.range.end()));
            if let (Some(x), Some(y)) = (g.map(a), g.map(b.saturating_sub(1))) {
                if y + 1 > x && (y + 1 - x) == b - a {
                    refs.push(Reference { file: *qdi, range: TextRange::new(x.into(), (y + 1).into()), target: r.target.clone() });
                }
            }
        }
    }
    for (file, span, p) in pending {
        if span == Span::default() {
            continue;
        }
        let target = match &p {
            Pending::Entity(e) => analysis.find_anywhere(e).map(SemaTarget::Decl),
            Pending::Member(e, m) => analysis.find_anywhere(e).and_then(|d| analysis.lookup_member(d, m)).map(|(d, i)| SemaTarget::Member(d, i)),
        };
        if let Some(t) = target {
            refs.push(Reference { file, range: TextRange::new(span.start.into(), span.end.into()), target: t });
        }
    }
    // the members the type checker found (`o.total`: sema does not know what `o` is), in the
    // code and in the interfaces
    let known: HashSet<(FileId, TextRange)> = analysis.references.iter().map(|r| (r.file, r.range)).chain(refs.iter().map(|r| (r.file, r.range))).collect();
    for (span, (entity, member)) in accesses {
        let target = analysis.find_anywhere(&entity).and_then(|d| analysis.lookup_member(d, &member)).map(|(d, i)| SemaTarget::Member(d, i));
        let Some(target) = target else { continue };
        let (file, range) = match by_gen.get(&span.file) {
            Some((qdi, g)) => {
                let (a, b) = (u32::from(span.range.start()), u32::from(span.range.end()));
                match (g.map(a), g.map(b.saturating_sub(1))) {
                    (Some(x), Some(y)) if y + 1 > x && y + 1 - x == b - a => (*qdi, TextRange::new(x.into(), (y + 1).into())),
                    _ => continue,
                }
            }
            None => (span.file, span.range),
        };
        if !known.contains(&(file, range)) {
            refs.push(Reference { file, range, target });
        }
    }
    analysis.references.extend(refs);
    // `o.total`: the message is about `total`, the name itself is where it is
    for d in diagnostics.iter_mut() {
        let parts: Vec<&str> = d.message.split('`').collect();
        if parts.len() >= 4 && parts[2] == " has no field " {
            let text = &project.file(d.file).text;
            let (a, b) = (usize::from(d.range.start()), usize::from(d.range.end()).min(text.len()));
            if let Some(t) = text.get(a..b) {
                if t.len() > parts[3].len() && t.ends_with(parts[3]) {
                    let start = (b - parts[3].len()) as u32;
                    d.range = TextRange::new(start.into(), (b as u32).into());
                }
            }
        }
    }
    // a name a migration renamed: the new name, as the fix (`quadra check --fix` applies it)
    let renames = migration_renames(project);
    if !renames.is_empty() {
        for d in diagnostics.iter_mut() {
            if let Some(m) = rename_hint(&renames, &d.message) {
                d.message = m;
            }
        }
    }
    // one message per place
    diagnostics.sort_by_key(|d| (d.file, d.range.start(), d.message.clone()));
    diagnostics.dedup_by(|a, b| a.file == b.file && a.range == b.range && a.message == b.message);
    Interfaces { project: extended, analysis, diagnostics, trees, generated, params }
}
