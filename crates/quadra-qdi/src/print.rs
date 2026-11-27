//! The canonical printer: one way to write a tree, the one the UI writes back and `format`
//! gives. Four spaces per level; a blank line between the items of a file, between the parts
//! of an app (its settings, filters, `let`s, each page) and between the blocks of a page; an
//! expression longer than the line is cut before each `.method(…)` that follows a `)`.

use crate::ast::*;
use crate::options::{self, Vt};
use crate::text::{block_ok, data, key, names, pair_key, quote, text_block};

/// Lines longer than this are cut where an expression allows it.
pub const WIDTH: usize = 100;

pub fn print(file: &File) -> String {
    let mut w = W { out: String::new() };
    for (k, item) in file.items.iter().enumerate() {
        if k > 0 {
            w.out.push('\n');
        }
        match item {
            Item::App(a) => w.app(a),
            Item::View(v) => w.saved_view(v),
        }
    }
    w.out
}

struct W {
    out: String,
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

/// An expression after `prefix` (already written at `indent`): one line, or cut before the
/// method calls of its outermost chain, each on a line of its own.
pub fn expr_text(prefix: usize, indent: usize, e: &Expr) -> String {
    if e.raw {
        return format!("raw {}", quote(&e.text));
    }
    let t = &e.text;
    if prefix + t.chars().count() <= WIDTH {
        return t.clone();
    }
    let cuts = chain_cuts(t);
    if cuts.is_empty() {
        return t.clone();
    }
    let mut out = String::new();
    let mut last = 0;
    for c in cuts {
        out.push_str(&t[last..c]);
        out.push('\n');
        out.push_str(&pad(indent + 4));
        last = c;
    }
    out.push_str(&t[last..]);
    out
}

/// Where an expression may be cut: before a `.` that follows a `)` outside brackets and
/// strings, when the expression is one chain of calls (no space outside brackets: not an
/// operation, a condition, a lambda).
fn chain_cuts(t: &str) -> Vec<usize> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut k = 0;
    while k < b.len() {
        match b[k] {
            b' ' if depth == 0 => return Vec::new(),
            b'"' => {
                k += 1;
                while k < b.len() && b[k] != b'"' {
                    if b[k] == b'\\' {
                        k += 1;
                    }
                    k += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 && b[k] == b')' && b.get(k + 1) == Some(&b'.') {
                    out.push(k + 1);
                }
            }
            _ => {}
        }
        k += 1;
    }
    out
}

impl W {
    fn line(&mut self, indent: usize, text: &str) {
        self.out.push_str(&pad(indent));
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    fn comment(&mut self, indent: usize, c: &Option<String>) {
        if let Some(c) = c {
            for l in c.split('\n') {
                if l.is_empty() {
                    self.line(indent, "#");
                } else {
                    self.line(indent, &format!("# {l}"));
                }
            }
        }
    }

    fn raw(&mut self, indent: usize, r: &Raw) {
        let path: Vec<String> = r.path.iter().map(|s| key(s)).collect();
        self.line(
            indent,
            &format!(
                "{} {}: {}",
                if r.meta { "meta" } else { "config" },
                path.join("."),
                data(&r.value)
            ),
        );
    }

    // ---- apps ----------------------------------------------------------------------------------

    fn app(&mut self, a: &App) {
        self.comment(0, &a.comment);
        let empty = a.description.is_none()
            && a.shares.is_empty()
            && a.filters.is_empty()
            && a.lets.is_empty()
            && a.pages.is_empty()
            && a.raws.is_empty();
        if empty {
            self.line(0, &format!("app {}", quote(&a.name)));
            return;
        }
        self.line(0, &format!("app {}:", quote(&a.name)));
        let mut first = true;
        let mut sep = |w: &mut W| {
            if !first {
                w.blank();
            }
            first = false;
        };
        if a.description.is_some() || !a.shares.is_empty() {
            sep(self);
            if let Some(d) = &a.description {
                self.line(4, &format!("description: {}", quote(d)));
            }
            if !a.shares.is_empty() {
                self.line(4, &format!("share: {}", shares(&a.shares)));
            }
        }
        if !a.filters.is_empty() {
            sep(self);
            for f in &a.filters {
                self.comment(4, &f.comment);
                let mut s = format!("filter {}", f.name.text);
                if let Some(l) = &f.label {
                    s.push(' ');
                    s.push_str(&quote(l));
                }
                s.push_str(&format!(
                    ": {} {}.{}",
                    f.widget.text, f.entity.text, f.field.text
                ));
                if let Some(d) = &f.default {
                    s.push_str(" default ");
                    s.push_str(&data(d));
                }
                self.line(4, &s);
                for r in &f.raws {
                    self.raw(8, r);
                }
            }
        }
        if !a.lets.is_empty() {
            sep(self);
            for l in &a.lets {
                self.comment(4, &l.comment);
                let prefix = format!("let {} = ", l.name.text);
                let e = expr_text(4 + prefix.len(), 4, &l.expr);
                self.line(4, &format!("{prefix}{e}"));
            }
        }
        for p in &a.pages {
            sep(self);
            self.page(p);
        }
        if !a.raws.is_empty() {
            sep(self);
            for r in &a.raws {
                self.raw(4, r);
            }
        }
    }

    fn page(&mut self, p: &Page) {
        self.comment(4, &p.comment);
        let mut head = "page".to_string();
        if let Some(id) = &p.id {
            head.push(' ');
            head.push_str(&id.text);
        }
        if let Some(n) = &p.name {
            head.push(' ');
            head.push_str(&quote(n));
        }
        if p.body.is_empty() && p.raws.is_empty() {
            self.line(4, &head);
            return;
        }
        self.line(4, &format!("{head}:"));
        for (k, b) in p.body.iter().enumerate() {
            if k > 0 {
                self.blank();
            }
            self.block(8, b);
        }
        for r in &p.raws {
            self.raw(8, r);
        }
    }

    fn block(&mut self, indent: usize, b: &Block) {
        match b {
            Block::Space(n, _) => self.line(indent, &format!("space {n}")),
            Block::Row(r) => {
                let head = match r.height {
                    Some(h) => format!("row height {h}:"),
                    None => "row:".to_string(),
                };
                self.line(indent, &head);
                for c in &r.children {
                    self.block(indent + 4, c);
                }
            }
            Block::Column(c) => {
                let head = match c.width {
                    Some(w) => format!("column width {w}:"),
                    None => "column:".to_string(),
                };
                self.line(indent, &head);
                for ch in &c.children {
                    self.block(indent + 4, ch);
                }
            }
            Block::Component(c) => self.component(indent, c),
        }
    }

    fn component(&mut self, indent: usize, c: &Component) {
        self.comment(indent, &c.comment);
        let mut head = match options::kind_word(&c.kind) {
            Some(k) => k.to_string(),
            None => format!("component {}", c.kind),
        };
        if let Some(n) = &c.name {
            head.push(' ');
            head.push_str(&n.text);
        }
        if let Some(t) = &c.title {
            head.push(' ');
            head.push_str(&quote(t));
        }
        if let Some(w) = c.width {
            head.push_str(&format!(" width {w}"));
        }
        if let Some(h) = c.height {
            head.push_str(&format!(" height {h}"));
        }
        if let Some([x, y, w, h]) = c.at {
            head.push_str(&format!(" at {x}, {y} size {w} x {h}"));
        }
        if c.lines.is_empty() {
            self.line(indent, &head);
            return;
        }
        self.line(indent, &format!("{head}:"));
        for l in &c.lines {
            self.body_line(indent + 4, &c.kind, l);
        }
    }

    fn saved_view(&mut self, v: &SavedView) {
        self.comment(0, &v.comment);
        let head = format!("view {} of {}", quote(&v.name), v.entity.text);
        if v.lines.is_empty() {
            self.line(0, &head);
            return;
        }
        self.line(0, &format!("{head}:"));
        for l in &v.lines {
            self.body_line(4, "", l);
        }
    }

    fn body_line(&mut self, indent: usize, kind: &str, l: &Line) {
        match l {
            Line::Opt { key, value, .. } => {
                let vt = if kind.is_empty() {
                    options::find_saved(&key.text).map(|o| o.vt)
                } else {
                    options::find(kind, &key.text).map(|(o, _)| o.vt)
                };
                let prefix = format!("{}: ", key.text);
                let v = value_text(indent + prefix.len(), indent, value, vt);
                self.line(indent, &format!("{prefix}{v}"));
            }
            Line::Apply { entity, binds, .. } => {
                let bs: Vec<String> = binds
                    .iter()
                    .map(|(n, p)| {
                        format!(
                            "{} -> {}",
                            n.text,
                            p.as_ref().map_or("none".to_string(), |p| p.text.clone())
                        )
                    })
                    .collect();
                self.line(indent, &format!("apply {}: {}", entity.text, bs.join(", ")));
            }
            Line::ApplyAuto(b, _) => self.line(indent, &format!("apply auto: {b}")),
            Line::Compute { name, expr, .. } => {
                let prefix = format!("compute {}: ", name.text);
                let e = expr_text(indent + prefix.len(), indent, expr);
                self.line(indent, &format!("{prefix}{e}"));
            }
            Line::Color { color, expr, .. } => {
                let prefix = format!("color {}: ", quote(color));
                let e = expr_text(indent + prefix.len(), indent, expr);
                self.line(indent, &format!("{prefix}{e}"));
            }
            Line::Default { field, source, .. } => self.line(
                indent,
                &format!("default {}: {}", field.text, source_text(source)),
            ),
            Line::Section { title, fields, .. } => {
                let mut s = "section".to_string();
                if let Some(t) = title {
                    s.push(' ');
                    s.push_str(&quote(t));
                }
                s.push(':');
                if !fields.is_empty() {
                    s.push(' ');
                    s.push_str(&names(fields));
                }
                self.line(indent, &s);
            }
            Line::Field(f) => {
                self.comment(indent, &f.comment);
                let mut s = format!("field {}", f.name.text);
                if let Some(l) = &f.label {
                    s.push(' ');
                    s.push_str(&quote(l));
                }
                s.push_str(": ");
                s.push_str(&f.ty.text);
                if let Some(wh) = &f.where_ {
                    s.push_str(" where ");
                    s.push_str(&wh.text);
                }
                if let Some(d) = &f.default {
                    s.push_str(" default ");
                    let e = expr_text(indent + s.len(), indent, d);
                    s.push_str(&e);
                }
                self.line(indent, &s);
            }
            Line::Submit { label, code, .. } => {
                let mut s = "submit".to_string();
                if let Some(l) = label {
                    s.push(' ');
                    s.push_str(&quote(l));
                }
                s.push(':');
                self.line(indent, &s);
                for l in code.split('\n') {
                    if l.is_empty() {
                        self.blank();
                    } else {
                        self.line(indent + 4, l);
                    }
                }
            }
            Line::Tab(t) => self.tab(indent, t),
            Line::Text(t, _) => {
                if block_ok(t) {
                    self.out.push_str(&text_block(t, indent));
                    self.out.push('\n');
                } else {
                    self.line(indent, &quote(t));
                }
            }
            Line::Raw(r) => self.raw(indent, r),
        }
    }

    fn tab(&mut self, indent: usize, t: &Tab) {
        self.comment(indent, &t.comment);
        let mut head = "tab".to_string();
        if let Some(id) = &t.id {
            head.push(' ');
            head.push_str(&id.text);
        }
        if let Some(n) = &t.name {
            head.push(' ');
            head.push_str(&quote(n));
        }
        if let Some(m) = &t.mode {
            head.push(' ');
            head.push_str(m);
        }
        match (t.rows, t.cols) {
            (Some(r), Some(c)) => head.push_str(&format!(" {r} x {c}")),
            (Some(r), None) => head.push_str(&format!(" rows {r}")),
            (None, Some(c)) => head.push_str(&format!(" cols {c}")),
            (None, None) => {}
        }
        if t.lines.is_empty() {
            self.line(indent, &head);
            return;
        }
        self.line(indent, &format!("{head}:"));
        let i = indent + 4;
        for l in &t.lines {
            match l {
                TabLine::Cell {
                    addr, style, value, ..
                } => {
                    let mut s = addr.clone();
                    for st in style {
                        s.push(' ');
                        s.push_str(&style_text(st));
                    }
                    s.push(':');
                    match value {
                        Some(CellValue::Text(v)) => {
                            s.push(' ');
                            s.push_str(&quote(v));
                        }
                        Some(CellValue::Formula(f, _)) => {
                            s.push_str(" = ");
                            s.push_str(f);
                        }
                        None => {}
                    }
                    self.line(i, &s);
                }
                TabLine::Widths(p, _) => self.line(i, &format!("width {}", pairs(p))),
                TabLine::Heights(p, _) => self.line(i, &format!("height {}", pairs(p))),
                TabLine::Merge(m, _) => self.line(i, &format!("merge {}", m.join(", "))),
                TabLine::Freeze { rows, cols, .. } => {
                    let mut s = "freeze".to_string();
                    if let Some(r) = rows {
                        s.push_str(&format!(" rows {r}"));
                    }
                    if let Some(c) = cols {
                        s.push_str(&format!(" cols {c}"));
                    }
                    self.line(i, &s);
                }
                TabLine::Raw(r) => self.raw(i, r),
            }
        }
    }
}

fn style_text(s: &StyleItem) -> String {
    match s {
        StyleItem::Flag(f) => f.clone(),
        StyleItem::Color(k, c) => format!("{k} {}", quote(c)),
        StyleItem::Align(k, a) => format!("{k} {a}"),
        StyleItem::Format {
            kind,
            dec,
            thousands,
            date,
        } => {
            let mut s = format!("fmt {kind}");
            if let Some(d) = dec {
                s.push_str(&format!(" dec {d}"));
            }
            if *thousands {
                s.push_str(" thousands");
            }
            if let Some(d) = date {
                s.push_str(&format!(" date {}", quote(d)));
            }
            s
        }
        StyleItem::Border(j) => format!("border {}", data(j)),
        StyleItem::Style(j) => format!("style {}", data(j)),
    }
}

pub fn shares(s: &[Share]) -> String {
    s.iter()
        .map(|s| {
            let who = match &s.role {
                Some(r) => format!("role {r}"),
                None => "everyone".to_string(),
            };
            format!("{who} {}", if s.change { "change" } else { "use" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn pairs(p: &[(String, serde_json::Value)]) -> String {
    p.iter()
        .map(|(k, v)| format!("{} {}", pair_key(k), data(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn source_text(s: &Source) -> String {
    match s {
        Source::Param(n) => format!("@{n}"),
        Source::Id(n) => format!("id {n}"),
        Source::Cell { component, cell } => format!("cell {component} {}", quote(cell)),
        Source::Value(v) => data(v),
    }
}

fn word(w: &str) -> String {
    if crate::text::is_path(w) {
        w.to_string()
    } else {
        quote(w)
    }
}

fn value_text(prefix: usize, indent: usize, v: &Value, _vt: Option<Vt>) -> String {
    match v {
        Value::Expr(e) => expr_text(prefix, indent, e),
        Value::Text(t) => quote(t),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Word(w) => word(w),
        Value::Names(n) => names(n),
        Value::Pairs(p) => pairs(p),
        Value::Data(d) => data(d),
        Value::Source(DataSource::Expr(e)) => expr_text(prefix, indent, e),
        Value::Source(DataSource::Cell { component, cell }) => {
            format!("cell {component} {}", quote(cell))
        }
        Value::Source(DataSource::Range { component, range }) => {
            format!("range {component} {}", quote(range))
        }
        Value::Compare(Compare::Previous(f)) => format!("previous @{f}"),
        Value::Compare(Compare::Target(e)) => {
            format!("target {}", expr_text(prefix + 7, indent, e))
        }
        Value::Run(r) => {
            let target = match &r.target {
                Target::Param(n) => format!("@{n}"),
                Target::Entity(e) => e.clone(),
                Target::Of(e, s) => {
                    let inner = match s {
                        Source::Id(n) => n.to_string(),
                        other => source_text(other),
                    };
                    format!("{e}[{inner}]")
                }
            };
            let args: Vec<String> = r
                .args
                .iter()
                .map(|(n, s)| format!("{n}={}", source_text(s)))
                .collect();
            format!("{target}.{}({})", r.action, args.join(", "))
        }
        Value::From(s) => source_text(s),
        Value::Sort(s) => s
            .iter()
            .map(|(f, d)| if *d { format!("{f} desc") } else { f.clone() })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Group(g) => g
            .iter()
            .map(|(f, b)| match b {
                Some(b) => format!("{f} by {b}"),
                None => f.clone(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Order(by, desc) => {
            if *desc {
                format!("{} desc", word(by))
            } else {
                word(by)
            }
        }
        Value::Shares(s) => shares(s),
        Value::Flags(f) => f.join(", "),
        Value::After(a) => match a {
            After::Clear => "clear".into(),
            After::Keep => "keep".into(),
            After::Open => "open".into(),
            After::Select { entity, name } => format!("select {entity} as {name}"),
        },
    }
}
