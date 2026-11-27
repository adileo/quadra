//! JSON definitions → a tree, for the printer: the inverse of lower.rs. What has a syntax is
//! written with it; what has none (a key the client added, a value of an unusual shape) is
//! written `config <key>: <data>` or `meta <key>: <data>`, so nothing is lost: lowering the
//! tree gives the JSON back (the interface files reference (quadra.run/reference/interfaces), "Round trip", lists the few normalizations).

use crate::ast::*;
use crate::layout::{self, Cut};
use crate::options::{self, Opt, Vt};
use crate::text::{is_ident, is_path};
use quadra_syntax::SyntaxKind as K;
use serde_json::{Map, Value as J};
use std::collections::{HashMap, HashSet};

pub fn lift(defs: &[J]) -> Result<(File, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut items = Vec::new();
    let mut apps = 0;
    for d in defs {
        let what = format!("`{}`", d["name"].as_str().unwrap_or("?"));
        for (k, _) in d.as_object().ok_or("a definition is an object")? {
            match k.as_str() {
                "kind" | "entity" | "type" | "name" | "description" | "config" | "shares" => {}
                "owner" => warnings.push(format!("{what}: its owner is not kept in a file (a file's apps and views belong to the project)")),
                other => warnings.push(format!("{what}: `{other}` is not kept in a file")),
            }
        }
        match d["kind"].as_str() {
            Some("app") => {
                apps += 1;
                if apps > 1 {
                    return Err("a file holds one app: write each app in a file of its own".into());
                }
                items.push(Item::App(
                    lift_app(d, &mut warnings).map_err(|e| format!("{what}: {e}"))?,
                ));
            }
            Some("view") => items.push(Item::View(
                lift_saved_view(d, &mut warnings).map_err(|e| format!("{what}: {e}"))?,
            )),
            other => {
                return Err(format!(
                    "{what}: a definition of kind app or view, not {other:?}"
                ))
            }
        }
    }
    Ok((File { items }, warnings))
}

// ---- what can be written without quotes -------------------------------------------------------------

fn lex(text: &str) -> Option<Vec<(K, String)>> {
    let (toks, errors) = quadra_syntax::lexer::tokenize(text);
    if errors
        .iter()
        .any(|e| !e.message.starts_with("unexpected character"))
    {
        return None;
    }
    let mut out = Vec::new();
    for t in toks {
        let s = text[t.range].to_string();
        match t.kind {
            K::WHITESPACE | K::NEWLINE | K::INDENT | K::DEDENT => {}
            K::COMMENT => return None,
            k => out.push((k, s)),
        }
    }
    Some(out)
}

/// `@` is only right before a name; brackets are balanced.
fn tokens_ok(text: &str, toks: &[(K, String)]) -> bool {
    let mut depth = 0i32;
    for (k, (kind, _)) in toks.iter().enumerate() {
        match kind {
            K::AT => {
                if !toks.get(k + 1).is_some_and(|(_, n)| is_ident(n)) {
                    return false;
                }
            }
            K::ERROR_TOKEN => return false,
            K::L_PAREN | K::L_BRACK | K::L_BRACE => depth += 1,
            K::R_PAREN | K::R_BRACK | K::R_BRACE => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !text.contains("@ ")
}

/// Can `text` be written as an expression (and read back the same)?
pub fn printable_expr(text: &str) -> bool {
    if text.is_empty() || text.trim() != text || text.contains('\n') || text.contains('\r') {
        return false;
    }
    let Some(toks) = lex(text) else { return false };
    if !tokens_ok(text, &toks) {
        return false;
    }
    // `raw "…"`, `cell x "…"`, `range x "…"` mean something else where an expression goes
    if toks.len() >= 2
        && matches!(toks[0].1.as_str(), "raw" | "cell" | "range")
        && toks.last().unwrap().0 == K::STRING
    {
        return false;
    }
    quadra_syntax::parse_expr(text).is_some()
}

fn expr(text: &str) -> Expr {
    Expr {
        text: text.to_string(),
        raw: !printable_expr(text),
        span: Span::default(),
        map: Vec::new(),
    }
}

/// A text written as it is up to the end of the line (a field's type, its `where`): one line,
/// no comment, brackets balanced, none of `stop` outside brackets, spaces as the parser joins
/// tokens.
fn printable_words(text: &str, stop: &[&str]) -> bool {
    if text.is_empty() || text.trim() != text || text.contains('\n') || text.contains('\r') {
        return false;
    }
    let Some(toks) = lex(text) else { return false };
    if !tokens_ok(text, &toks) || toks.iter().any(|(_, s)| stop.contains(&s.as_str())) {
        return false;
    }
    true
}

/// A field's type as the parser reads it back: its tokens, a space where there was a gap.
fn type_ok(text: &str) -> bool {
    if !printable_words(text, &["where", "default"]) {
        return false;
    }
    let (toks, _) = quadra_syntax::lexer::tokenize(text);
    let mut s = String::new();
    let mut last_end = None;
    for t in toks
        .iter()
        .filter(|t| !matches!(t.kind, K::WHITESPACE | K::NEWLINE | K::INDENT | K::DEDENT))
    {
        if let Some(e) = last_end {
            if t.range.start() > e {
                s.push(' ');
            }
        }
        s.push_str(&text[t.range]);
        last_end = Some(t.range.end());
    }
    s == text
}

/// A cell's formula after `=`, written as it is to the end of the line.
fn printable_formula(f: &str) -> bool {
    if f.is_empty() || f.trim() != f || f.contains('\n') || f.contains('\r') {
        return false;
    }
    let (toks, errors) = quadra_syntax::lexer::tokenize(f);
    if errors
        .iter()
        .any(|e| !e.message.starts_with("unexpected character"))
    {
        return false;
    }
    let sig: Vec<_> = toks
        .iter()
        .filter(|t| {
            !matches!(
                t.kind,
                K::WHITESPACE | K::COMMENT | K::NEWLINE | K::INDENT | K::DEDENT
            )
        })
        .collect();
    let mut depth = 0i32;
    for t in &sig {
        match t.kind {
            K::L_PAREN | K::L_BRACK | K::L_BRACE => depth += 1,
            K::R_PAREN | K::R_BRACK | K::R_BRACE => depth -= 1,
            _ => {}
        }
    }
    if depth != 0 {
        return false;
    }
    // a line ending with `,` or an operator goes on with the next one
    match sig.last() {
        None => false,
        Some(t) => {
            !matches!(
                t.kind,
                K::COMMA
                    | K::PLUS
                    | K::MINUS
                    | K::SLASH
                    | K::QUESTION2
                    | K::EQ2
                    | K::NEQ
                    | K::LE
                    | K::GE
                    | K::AND_KW
                    | K::OR_KW
            ) && &f[t.range] != "implies"
        }
    }
}

/// Statements of a custom form's `submit`, written as a block (and read back the same).
fn printable_code(code: &str) -> bool {
    if code.is_empty() || code.contains('\r') {
        return false;
    }
    let lines: Vec<&str> = code.split('\n').collect();
    if lines
        .first()
        .is_some_and(|l| l.trim().is_empty() || l.starts_with(' '))
        || lines.last().is_some_and(|l| l.trim().is_empty())
    {
        return false;
    }
    if lines.iter().any(|l| {
        l.trim_end() != *l
            || l.trim_start_matches(' ').starts_with('\t')
            || (l.trim().is_empty() && !l.is_empty())
    }) {
        return false;
    }
    let (toks, errors) = quadra_syntax::lexer::tokenize(code);
    if errors
        .iter()
        .any(|e| !e.message.starts_with("unexpected character"))
    {
        return false;
    }
    let mut depth = 0i32;
    for t in &toks {
        match t.kind {
            K::L_PAREN | K::L_BRACK | K::L_BRACE => depth += 1,
            K::R_PAREN | K::R_BRACK | K::R_BRACE => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

fn is_addr(w: &str) -> bool {
    let letters = w.chars().take_while(|c| c.is_ascii_uppercase()).count();
    (1..=3).contains(&letters)
        && w.len() > letters
        && w[letters..].chars().all(|c| c.is_ascii_digit())
        && !w[letters..].starts_with('0')
}

fn addr_key(a: &str) -> (u64, u64) {
    let letters: String = a.chars().take_while(|c| c.is_ascii_uppercase()).collect();
    let col = letters.chars().fold(0u64, |n, c| n * 26 + (c as u64 - 64));
    (a[letters.len()..].parse().unwrap_or(0), col)
}

// ---- what is used of an object -----------------------------------------------------------------------

/// An object being written: the paths written with a syntax (or dropped as defaults); the rest
/// becomes `config`/`meta` lines.
struct Obj<'a> {
    j: &'a Map<String, J>,
    used: Vec<Vec<String>>,
}

impl<'a> Obj<'a> {
    fn new(j: &'a Map<String, J>) -> Obj<'a> {
        Obj {
            j,
            used: Vec::new(),
        }
    }

    fn get(&self, path: &str) -> Option<&'a J> {
        let mut cur: &J = self.j.get(path.split('.').next().unwrap())?;
        for seg in path.split('.').skip(1) {
            cur = cur.as_object()?.get(seg)?;
        }
        Some(cur)
    }

    fn is_used(&self, path: &str) -> bool {
        let p: Vec<&str> = path.split('.').collect();
        self.used
            .iter()
            .any(|u| u.len() <= p.len() && u.iter().zip(&p).all(|(a, b)| a == b))
    }

    fn use_(&mut self, path: &str) {
        self.used
            .push(path.split('.').map(str::to_string).collect());
    }

    fn raws(&self, meta: bool) -> Vec<Raw> {
        let mut out = Vec::new();
        walk(self.j, &mut Vec::new(), &self.used, meta, &mut out);
        out
    }
}

fn walk(
    j: &Map<String, J>,
    prefix: &mut Vec<String>,
    used: &[Vec<String>],
    meta: bool,
    out: &mut Vec<Raw>,
) {
    for (k, v) in j {
        prefix.push(k.clone());
        if used.iter().any(|u| u == prefix) {
        } else if let (true, Some(o)) = (
            used.iter()
                .any(|u| u.len() > prefix.len() && u.starts_with(prefix)),
            v.as_object(),
        ) {
            walk(o, prefix, used, meta, out);
        } else {
            out.push(Raw {
                meta,
                path: prefix.clone(),
                value: v.clone(),
                span: Span::default(),
            });
        }
        prefix.pop();
    }
}

fn opt(key: &str, value: Value) -> Line {
    Line::Opt {
        key: Name::new(key),
        value,
        span: Span::default(),
        names: Vec::new(),
    }
}

/// A simple option's value from the JSON (`None`: it has another shape).
fn simple(vt: Vt, v: &J) -> Option<Value> {
    Some(match vt {
        Vt::Text => Value::Text(v.as_str()?.to_string()),
        Vt::Int => Value::Int(v.as_i64()?),
        Vt::Bool => Value::Bool(v.as_bool()?),
        Vt::Word(set) => {
            let s = v.as_str()?;
            if (!set.is_empty() && !set.contains(&s)) || s.is_empty() {
                return None;
            }
            Value::Word(s.to_string())
        }
        Vt::Names => Value::Names(
            v.as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
        ),
        Vt::Pairs => {
            let o = v.as_object()?;
            if o.values().any(|x| x.is_object() || x.is_array()) || o.keys().any(|k| k.is_empty()) {
                return None;
            }
            Value::Pairs(o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        }
        Vt::Expr => Value::Expr(expr(v.as_str()?)),
        Vt::Sort => Value::Sort(
            v.as_array()?
                .iter()
                .map(|x| {
                    let o = x.as_object()?;
                    if o.len() != 2 {
                        return None;
                    }
                    let f = o.get("field")?.as_str().filter(|f| is_path(f))?;
                    Some((f.to_string(), o.get("desc")?.as_bool()?))
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        Vt::From => Value::From(source(v)?),
        Vt::Source => Value::Source(match v["type"].as_str()? {
            "expr" if v.as_object()?.len() == 2 => DataSource::Expr(expr(v["expr"].as_str()?)),
            "cell" if v.as_object()?.len() == 3 && is_ident(v["component"].as_str()?) => {
                DataSource::Cell {
                    component: v["component"].as_str()?.into(),
                    cell: v["cell"].as_str()?.into(),
                }
            }
            "range" if v.as_object()?.len() == 3 && is_ident(v["component"].as_str()?) => {
                DataSource::Range {
                    component: v["component"].as_str()?.into(),
                    range: v["range"].as_str()?.into(),
                }
            }
            _ => return None,
        }),
        Vt::Compare => Value::Compare(match v["kind"].as_str()? {
            "previous_period" if v.as_object()?.len() == 2 && is_ident(v["filter"].as_str()?) => {
                Compare::Previous(v["filter"].as_str()?.into())
            }
            "target" if v.as_object()?.len() == 2 => Compare::Target(expr(v["expr"].as_str()?)),
            _ => return None,
        }),
        Vt::Flags(set) => {
            let o = v.as_object()?;
            if o.is_empty()
                || o.iter()
                    .any(|(k, x)| !set.contains(&k.as_str()) || x != &J::Bool(true))
            {
                return None;
            }
            // in the order of the set
            Value::Flags(
                set.iter()
                    .filter(|k| o.contains_key(**k))
                    .map(|k| k.to_string())
                    .collect(),
            )
        }
        Vt::Shares | Vt::Run | Vt::Group | Vt::Order | Vt::After => return None,
    })
}

/// A `{from: …}` source.
fn source(v: &J) -> Option<Source> {
    let o = v.as_object()?;
    Some(match o.get("from")?.as_str()? {
        "param" if o.len() == 2 && is_ident(o.get("name")?.as_str()?) => {
            Source::Param(o["name"].as_str()?.into())
        }
        "id" if o.len() == 2 => Source::Id(o.get("id")?.as_u64()?),
        "cell" if o.len() == 3 && is_ident(o.get("component")?.as_str()?) => Source::Cell {
            component: o["component"].as_str()?.into(),
            cell: o.get("cell")?.as_str()?.into(),
        },
        "value" if o.len() == 2 && o.contains_key("value") => Source::Value(o["value"].clone()),
        _ => return None,
    })
}

/// Options of a table, in order: those with a simple value, written; defaults dropped.
fn simple_opts(
    o: &mut Obj,
    ty: &str,
    table: &[Opt],
    prefix: &str,
    skip: &[&str],
    out: &mut Vec<Line>,
) {
    for t in table {
        if skip.contains(&t.key) {
            continue;
        }
        let path = format!("{prefix}{}", t.path);
        let Some(v) = o.get(&path) else { continue };
        if o.is_used(&path) {
            continue;
        }
        if options::is_default(ty, t.path, v) {
            o.use_(&path);
            continue;
        }
        if let Some(val) = simple(t.vt, v) {
            o.use_(&path);
            out.push(opt(t.key, val));
        }
    }
}

// ---- apps ------------------------------------------------------------------------------------------

fn lift_shares(s: &J, warnings: &mut Vec<String>) -> Vec<Share> {
    let mut out = Vec::new();
    for s in s.as_array().into_iter().flatten() {
        let change = s["edit"].as_bool().unwrap_or(false);
        match s["type"].as_str() {
            Some("role") if s["name"].as_str().is_some_and(is_ident) => out.push(Share { role: Some(s["name"].as_str().unwrap().into()), change }),
            Some("everyone") => out.push(Share { role: None, change }),
            Some(t @ ("user" | "team")) => warnings.push(format!("a share with a {t} is not kept in a file (share it in the app, on top of the file's)")),
            _ => warnings.push(format!("a share {s} is not kept in a file")),
        }
    }
    out
}

fn comment_of(o: &mut Obj) -> Option<String> {
    let c = o.get("comment")?.as_str()?.to_string();
    o.use_("comment");
    Some(c)
}

fn lift_app(d: &J, warnings: &mut Vec<String>) -> Result<App, String> {
    let name = d["name"].as_str().ok_or("an app has a name")?.to_string();
    let description = d["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let shares = lift_shares(&d["shares"], warnings);
    let cfg = d["config"].as_object().ok_or("`config`: an object")?;
    let mut o = Obj::new(cfg);
    let comment = comment_of(&mut o);
    if cfg.get("version").is_some_and(|v| v == 1) {
        o.use_("version");
    }
    // lets
    let mut lets = Vec::new();
    if let Some(ls) = cfg.get("lets").and_then(J::as_array) {
        let typed: Option<Vec<Let>> = ls
            .iter()
            .map(|l| {
                let m = l.as_object()?;
                if m.keys()
                    .any(|k| !matches!(k.as_str(), "name" | "expr" | "comment"))
                {
                    return None;
                }
                let n = m.get("name")?.as_str().filter(|n| is_ident(n))?;
                Some(Let {
                    comment: m
                        .get("comment")
                        .and_then(|c| c.as_str())
                        .map(str::to_string),
                    name: Name::new(n),
                    expr: expr(m.get("expr")?.as_str()?),
                    span: Span::default(),
                })
            })
            .collect();
        if let Some(t) = typed {
            lets = t;
            o.use_("lets");
        }
    }
    let pages_j = cfg
        .get("pages")
        .and_then(J::as_array)
        .ok_or("`pages`: a list of pages")?;
    o.use_("pages");
    // the components of each page, in the order of the grid
    let mut all: Vec<Vec<(usize, &J, [u32; 4])>> = Vec::new();
    for p in pages_j {
        let mut list = Vec::new();
        for (k, c) in p["components"].as_array().into_iter().flatten().enumerate() {
            let l = &c["layout"];
            let n = |k: &str| l[k].as_u64().map(|x| x as u32);
            let r = match (n("x"), n("y"), n("w"), n("h")) {
                (Some(x), Some(y), Some(w), Some(h))
                    if w >= 1 && x + w <= layout::COLUMNS && h >= 1 =>
                {
                    [x, y, w, h]
                }
                _ => {
                    return Err(format!(
                        "component `{}`: `layout` is not on the grid",
                        c["id"].as_str().unwrap_or("?")
                    ))
                }
            };
            list.push((k, c, r));
        }
        list.sort_by_key(|(k, _, r)| (r[1], r[0], *k));
        all.push(list);
    }
    // the selections
    let mut selections: HashMap<String, String> = HashMap::new();
    for list in &all {
        for (_, c, _) in list {
            let k = &c["config"];
            if c["type"] == "view" {
                if let (Some(s), Some(e)) = (k["select"].as_str(), k["entity"].as_str()) {
                    selections.entry(s.into()).or_insert(e.into());
                }
            }
            if c["type"] == "form" && k["after"] == "select" {
                if let (Some(s), Some(e)) =
                    (k["select"]["name"].as_str(), k["select"]["entity"].as_str())
                {
                    selections.entry(s.into()).or_insert(e.into());
                }
            }
        }
    }
    // the filters, declared once for the app, in the order they are first defined
    let mut filters: Vec<FilterDecl> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for list in &all {
        for (_, c, _) in list {
            if c["type"] != "filter" {
                continue;
            }
            for f in c["config"]["filters"].as_array().into_iter().flatten() {
                let fo = f.as_object().ok_or("a filter is an object")?;
                let mut fob = Obj::new(fo);
                let name = fo
                    .get("name")
                    .and_then(J::as_str)
                    .filter(|n| is_ident(n))
                    .ok_or("a filter's `name`")?;
                if !seen.insert(name.to_string()) {
                    return Err(format!("two filters named `{name}`"));
                }
                let entity = fo
                    .get("entity")
                    .and_then(J::as_str)
                    .filter(|n| is_ident(n))
                    .ok_or(format!("filter `{name}`: `entity`"))?;
                let field = fo
                    .get("field")
                    .and_then(J::as_str)
                    .filter(|n| is_path(n))
                    .ok_or(format!("filter `{name}`: `field`"))?;
                let widget = fo
                    .get("widget")
                    .and_then(J::as_str)
                    .filter(|n| is_ident(n))
                    .ok_or(format!("filter `{name}`: `widget`"))?;
                for k in ["name", "entity", "field", "widget"] {
                    fob.use_(k);
                }
                let label = fo.get("label").and_then(J::as_str).map(str::to_string);
                if label.is_some() {
                    fob.use_("label");
                }
                let default = fo.get("default").cloned();
                if default.is_some() {
                    fob.use_("default");
                }
                let comment = comment_of(&mut fob);
                filters.push(FilterDecl {
                    comment,
                    name: Name::new(name),
                    label,
                    widget: Name::new(widget),
                    entity: Name::new(entity),
                    field: Name::new(field),
                    default,
                    raws: fob.raws(true),
                    span: Span::default(),
                });
            }
        }
    }
    let mut pages = Vec::new();
    for (p, list) in pages_j.iter().zip(&all) {
        let po = p.as_object().ok_or("a page is an object")?;
        let mut pob = Obj::new(po);
        pob.use_("components");
        let id = match po.get("id").and_then(J::as_str) {
            Some(i) if is_ident(i) => {
                pob.use_("id");
                Some(Name::new(i))
            }
            _ => None,
        };
        let name = po.get("name").and_then(J::as_str).map(str::to_string);
        if name.is_some() {
            pob.use_("name");
        }
        let comment = comment_of(&mut pob);
        let mut comps: Vec<Option<Component>> = Vec::new();
        let mut rects = Vec::new();
        for (n, (_, c, r)) in list.iter().enumerate() {
            comps.push(Some(lift_component(c, &selections)?));
            rects.push((n, *r));
        }
        let cuts = layout::cut_page(&rects);
        let body = blocks(cuts, &mut comps);
        pages.push(Page {
            comment,
            id,
            name,
            body,
            raws: pob.raws(true),
            span: Span::default(),
        });
    }
    Ok(App {
        comment,
        name,
        description,
        shares,
        filters,
        lets,
        pages,
        raws: o.raws(true),
        span: Span::default(),
    })
}

fn blocks(cuts: Vec<Cut>, comps: &mut [Option<Component>]) -> Vec<Block> {
    cuts.into_iter()
        .map(|c| match c {
            Cut::Row { height, children } => Block::Row(Row {
                height: Some(height),
                children: blocks(children, comps),
                span: Span::default(),
            }),
            Cut::Column { width, children } => Block::Column(Column {
                width: Some(width),
                children: blocks(children, comps),
                span: Span::default(),
            }),
            Cut::Space(n) => Block::Space(n, Span::default()),
            Cut::Comp {
                idx,
                width,
                height,
                at,
            } => {
                let mut c = comps[idx].take().expect("a component is placed once");
                c.width = width;
                c.height = height;
                c.at = at;
                Block::Component(Box::new(c))
            }
        })
        .collect()
}

fn lift_component(c: &J, selections: &HashMap<String, String>) -> Result<Component, String> {
    let co = c.as_object().ok_or("a component is an object")?;
    let mut env = Obj::new(co);
    let id = co
        .get("id")
        .and_then(J::as_str)
        .ok_or("a component has an `id`")?;
    let kind = co
        .get("type")
        .and_then(J::as_str)
        .filter(|t| is_ident(t))
        .ok_or(format!("component `{id}`: `type`"))?
        .to_string();
    env.use_("type");
    env.use_("layout");
    let name = if is_ident(id) {
        env.use_("id");
        Some(Name::new(id))
    } else {
        None
    };
    let title = co.get("title").and_then(J::as_str).map(str::to_string);
    if title.is_some() {
        env.use_("title");
    }
    let comment = comment_of(&mut env);
    let mut lines = Vec::new();
    let mut config_raws = Vec::new();
    if let Some(cfg) = co.get("config").and_then(J::as_object) {
        env.use_("config");
        let mut o = Obj::new(cfg);
        // nulls are what is not set
        for (k, v) in cfg {
            if v.is_null() {
                o.use_(k);
            }
        }
        component_lines(&kind, &mut o, selections, &mut lines);
        config_raws = o.raws(false);
    }
    // the filters it applies
    if let Some(b) = co.get("bindings") {
        if let Some(bo) = b.as_object() {
            {
                if bo.is_empty() {
                    env.use_("bindings");
                }
                if let Some(f) = bo.get("filters").and_then(J::as_object) {
                    for (e, m) in f {
                        if e == "auto" {
                            if let Some(v) = m.as_bool() {
                                lines.push(Line::ApplyAuto(v, Span::default()));
                                env.use_("bindings.filters.auto");
                            }
                            continue;
                        }
                        let Some(mm) = m.as_object().filter(|mm| !mm.is_empty() && is_ident(e))
                        else {
                            continue;
                        };
                        let binds: Option<Vec<(Name, Option<Name>)>> = mm
                            .iter()
                            .map(|(n, p)| {
                                if !is_ident(n) {
                                    return None;
                                }
                                match p {
                                    J::Null => Some((Name::new(n), None)),
                                    J::String(s) if is_path(s) => {
                                        Some((Name::new(n), Some(Name::new(s))))
                                    }
                                    _ => None,
                                }
                            })
                            .collect();
                        if let Some(binds) = binds {
                            lines.push(Line::Apply {
                                entity: Name::new(e),
                                binds,
                                span: Span::default(),
                            });
                            env.use_(&format!("bindings.filters.{e}"));
                        }
                    }
                    if f.is_empty() {
                        env.use_("bindings.filters");
                    }
                }
            }
        }
    }
    lines.extend(config_raws.into_iter().map(Line::Raw));
    let mut meta = env.raws(true);
    if name.is_none() {
        meta.retain(|r| r.path != ["id"]);
        meta.insert(
            0,
            Raw {
                meta: true,
                path: vec!["id".into()],
                value: J::String(id.into()),
                span: Span::default(),
            },
        );
    }
    lines.extend(meta.into_iter().map(Line::Raw));
    Ok(Component {
        comment,
        kind,
        name,
        title,
        width: None,
        height: None,
        at: None,
        lines,
        span: Span::default(),
    })
}

/// The lines of a component's configuration, by kind.
fn component_lines(
    kind: &str,
    o: &mut Obj,
    selections: &HashMap<String, String>,
    out: &mut Vec<Line>,
) {
    match kind {
        "markdown" => {
            if let Some(t) = o.get("text").and_then(J::as_str) {
                o.use_("text");
                out.push(Line::Text(t.to_string(), Span::default()));
            }
        }
        "kpi" => simple_opts(o, kind, options::KPI, "", &[], out),
        "chart" => simple_opts(o, kind, options::CHART, "", &[], out),
        "filter" => {
            let mut names: Vec<String> = Vec::new();
            let defs = o.get("filters").and_then(J::as_array);
            let defs_ok = defs.is_none_or(|d| d.iter().all(|f| f["name"].as_str().is_some()));
            let show = o.get("show").and_then(J::as_array);
            let show_ok = show.is_none_or(|s| s.iter().all(|n| n.as_str().is_some_and(is_ident)));
            if defs_ok && show_ok {
                for f in defs.into_iter().flatten() {
                    names.push(f["name"].as_str().unwrap().to_string());
                }
                for n in show.into_iter().flatten() {
                    names.push(n.as_str().unwrap().to_string());
                }
                o.use_("filters");
                o.use_("show");
                if !names.is_empty() {
                    out.push(opt("show", Value::Names(names)));
                }
            }
            simple_opts(o, kind, options::FILTERS, "", &["show"], out);
        }
        "button" => {
            simple_opts(o, kind, &options::BUTTON[..1], "", &[], out);
            if let Some(run) = run_of(o, selections) {
                out.push(opt("run", Value::Run(run)));
            }
            if o.get("args")
                .is_some_and(|a| options::is_default("button", "args", a))
            {
                o.use_("args");
            }
            simple_opts(o, kind, &options::BUTTON[2..], "", &[], out);
        }
        "form" => form_lines(o, out),
        "view" => {
            simple_opts(o, kind, options::VIEW, "", &[], out);
            if let Some(v) = o.get("view").and_then(J::as_object) {
                if !v.is_empty() {
                    view_body(o, "view.", out);
                }
            }
        }
        "sheet" => {
            simple_opts(o, kind, options::SHEET, "", &[], out);
            if let Some(sheets) = o.get("sheets").and_then(J::as_array) {
                if sheets.iter().all(J::is_object) {
                    o.use_("sheets");
                    for s in sheets {
                        out.push(Line::Tab(lift_tab(s.as_object().unwrap())));
                    }
                }
            }
        }
        _ => {}
    }
}

fn run_of(o: &mut Obj, selections: &HashMap<String, String>) -> Option<Run> {
    let entity = o
        .get("entity")
        .and_then(J::as_str)
        .filter(|e| is_ident(e))?
        .to_string();
    let action = o
        .get("action")
        .and_then(J::as_str)
        .filter(|e| is_ident(e))?
        .to_string();
    let target = match o.get("target") {
        None | Some(J::Null) => Target::Entity(entity.clone()),
        Some(t) => match source(t)? {
            Source::Param(n) if selections.get(&n) == Some(&entity) => Target::Param(n),
            s @ (Source::Param(_) | Source::Id(_) | Source::Cell { .. }) => {
                Target::Of(entity.clone(), s)
            }
            Source::Value(_) => return None,
        },
    };
    let mut args = Vec::new();
    match o.get("args") {
        None | Some(J::Null) => {}
        Some(a) => {
            for (k, v) in a.as_object()? {
                if !is_ident(k) {
                    return None;
                }
                args.push((k.clone(), source(v)?));
            }
        }
    }
    for k in ["entity", "action", "target", "args"] {
        o.use_(k);
    }
    Some(Run {
        target,
        action,
        args,
        span: Span::default(),
    })
}

fn form_lines(o: &mut Obj, out: &mut Vec<Line>) {
    let ty = "form";
    // custom: its fields and its statements
    let mut custom_lines = Vec::new();
    if let Some(fs) = o.get("fields").and_then(J::as_array) {
        let typed: Option<Vec<Line>> = fs
            .iter()
            .map(|f| {
                let m = f.as_object()?;
                if m.keys().any(|k| {
                    !matches!(
                        k.as_str(),
                        "name" | "type" | "label" | "where" | "default" | "comment"
                    )
                }) {
                    return None;
                }
                let name = m.get("name")?.as_str().filter(|n| is_ident(n))?;
                let ty = m.get("type")?.as_str().filter(|t| type_ok(t))?;
                let label = match m.get("label") {
                    None => None,
                    Some(l) => Some(l.as_str()?.to_string()),
                };
                let where_ = match m.get("where") {
                    None => None,
                    Some(w) => Some(Expr::new(
                        w.as_str().filter(|w| printable_words(w, &["default"]))?,
                    )),
                };
                let default = match m.get("default") {
                    None => None,
                    Some(d) => Some(expr(d.as_str()?)),
                };
                let comment = match m.get("comment") {
                    None => None,
                    Some(c) => Some(c.as_str()?.to_string()),
                };
                Some(Line::Field(FormField {
                    comment,
                    name: Name::new(name),
                    label,
                    ty: Name::new(ty),
                    where_,
                    default,
                    span: Span::default(),
                }))
            })
            .collect();
        if let Some(t) = typed {
            o.use_("fields");
            custom_lines = t;
        }
    }
    let submit = o
        .get("submit")
        .and_then(J::as_str)
        .filter(|c| printable_code(c))
        .map(str::to_string);
    let custom_typed = !custom_lines.is_empty() || submit.is_some();
    // entity, mode, record
    simple_opts(o, ty, &options::FORM[..1], "", &[], out);
    if let Some(m) = o.get("mode") {
        if (m == "custom" && custom_typed) || options::is_default(ty, "mode", m) {
            o.use_("mode");
        } else if let Some(v) = simple(Vt::Word(options::FORM_MODES), m) {
            o.use_("mode");
            out.push(opt("mode", v));
        }
    }
    simple_opts(o, ty, &options::FORM[2..3], "", &[], out);
    // sections
    if let Some(s) = o.get("sections") {
        if options::is_default(ty, "sections", s) {
            o.use_("sections");
        } else if let Some(arr) = s.as_array() {
            let secs: Option<Vec<(Option<String>, Vec<String>)>> = arr
                .iter()
                .map(|x| {
                    let m = x.as_object()?;
                    if m.keys().any(|k| k != "title" && k != "fields") {
                        return None;
                    }
                    let fields = m
                        .get("fields")?
                        .as_array()?
                        .iter()
                        .map(|f| f.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?;
                    let title = match m.get("title") {
                        None => None,
                        Some(t) => Some(t.as_str()?.to_string()),
                    };
                    Some((title, fields))
                })
                .collect();
            if let Some(secs) = secs {
                o.use_("sections");
                if secs.len() == 1 && secs[0].0.is_none() && !secs[0].1.is_empty() {
                    out.push(opt("fields", Value::Names(secs[0].1.clone())));
                } else {
                    for (title, fields) in secs {
                        out.push(Line::Section {
                            title,
                            fields,
                            span: Span::default(),
                            names: Vec::new(),
                        });
                    }
                }
            }
        }
    }
    // defaults
    if let Some(d) = o.get("defaults") {
        if options::is_default(ty, "defaults", d) {
            o.use_("defaults");
        } else if let Some(m) = d.as_object() {
            let ds: Option<Vec<Line>> = m
                .iter()
                .map(|(k, v)| {
                    if is_ident(k) {
                        Some(Line::Default {
                            field: Name::new(k.as_str()),
                            source: source(v)?,
                            span: Span::default(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(ds) = ds {
                o.use_("defaults");
                out.extend(ds);
            }
        }
    }
    out.extend(custom_lines);
    if let Some(code) = submit {
        o.use_("submit");
        let label = o
            .get("submit_label")
            .and_then(J::as_str)
            .map(str::to_string);
        if label.is_some() {
            o.use_("submit_label");
        }
        out.push(Line::Submit {
            label,
            code,
            span: Span::default(),
            code_span: Span::default(),
        });
    }
    // columns, submit_label, confirm
    simple_opts(o, ty, &options::FORM[4..7], "", &[], out);
    // after
    if let Some(a) = o.get("after").and_then(J::as_str) {
        let v = match a {
            "clear" => Some(After::Clear),
            "keep" => Some(After::Keep),
            "open" => Some(After::Open),
            "select" => {
                let s = o.get("select");
                match (
                    s.and_then(|s| s["name"].as_str()),
                    s.and_then(|s| s["entity"].as_str()),
                    s.and_then(J::as_object).map(|m| m.len()),
                ) {
                    (Some(n), Some(e), Some(2)) if is_ident(n) && is_ident(e) => {
                        o.use_("select");
                        Some(After::Select {
                            entity: e.into(),
                            name: n.into(),
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(v) = v {
            o.use_("after");
            out.push(opt("after", Value::After(v)));
        }
    }
    simple_opts(o, ty, &options::FORM[8..], "", &[], out);
}

/// The options of a view's rows (`prefix`: `view.` in an embedded view).
fn view_body(o: &mut Obj, prefix: &str, out: &mut Vec<Line>) {
    let ty = "view_body";
    let p = |k: &str| format!("{prefix}{k}");
    let body = options::VIEW_BODY;
    let by_key = |k: &str| *body.iter().find(|x| x.key == k).unwrap();
    // nulls are what is not set
    if let Some(v) = o
        .get(prefix.trim_end_matches('.'))
        .and_then(J::as_object)
        .filter(|_| !prefix.is_empty())
    {
        for (k, x) in v {
            if x.is_null() {
                o.use_(&p(k));
            }
        }
    }
    simple_opts(
        o,
        ty,
        &[by_key("fields"), by_key("widths"), by_key("where")],
        prefix,
        &[],
        out,
    );
    // computed columns, colors
    if let Some(c) = o.get(&p("columns")) {
        if options::is_default(ty, "columns", c) {
            o.use_(&p("columns"));
        } else if let Some(m) = c
            .as_object()
            .filter(|m| m.iter().all(|(k, v)| is_ident(k) && v.is_string()))
        {
            o.use_(&p("columns"));
            for (k, v) in m {
                out.push(Line::Compute {
                    name: Name::new(k.as_str()),
                    expr: expr(v.as_str().unwrap()),
                    span: Span::default(),
                });
            }
        }
    }
    if let Some(c) = o.get(&p("colors")) {
        if options::is_default(ty, "colors", c) {
            o.use_(&p("colors"));
        } else if let Some(a) = c.as_array() {
            let rules: Option<Vec<Line>> = a
                .iter()
                .map(|r| {
                    let m = r.as_object()?;
                    if m.len() != 2 {
                        return None;
                    }
                    Some(Line::Color {
                        color: m.get("color")?.as_str()?.to_string(),
                        expr: expr(m.get("expr")?.as_str()?),
                        span: Span::default(),
                    })
                })
                .collect();
            if let Some(r) = rules {
                o.use_(&p("colors"));
                out.extend(r);
            }
        }
    }
    simple_opts(o, ty, &[by_key("sort"), by_key("search")], prefix, &[], out);
    // the grouping, whole
    if let Some(g) = o.get(&p("group")).and_then(J::as_object) {
        let keys: Option<Vec<(String, Option<String>)>> =
            g.get("keys").and_then(J::as_array).and_then(|ks| {
                ks.iter()
                    .map(|k| {
                        let m = k.as_object()?;
                        if m.keys().any(|x| x != "field" && x != "bucket") {
                            return None;
                        }
                        let f = m.get("field")?.as_str().filter(|f| is_path(f))?.to_string();
                        let b = match m.get("bucket") {
                            None | Some(J::Null) => None,
                            Some(b) => Some(b.as_str().filter(|b| is_ident(b))?.to_string()),
                        };
                        Some((f, b))
                    })
                    .collect()
            });
        let aggs = g.get("aggregates").and_then(|a| simple(Vt::Pairs, a));
        let pivot = g.get("pivot").and_then(J::as_bool);
        let order = g
            .get("order")
            .and_then(J::as_object)
            .filter(|m| m.len() == 2)
            .and_then(|m| {
                Some((
                    m.get("by")?.as_str()?.to_string(),
                    m.get("desc")?.as_bool()?,
                ))
            });
        let measure = g
            .get("measure")
            .and_then(J::as_str)
            .filter(|m| !m.is_empty());
        if let (
            Some(keys),
            Some(Value::Pairs(aggs)),
            Some(pivot),
            Some((by, desc)),
            Some(measure),
            5,
        ) = (keys, aggs, pivot, order, measure, g.len())
        {
            o.use_(&p("group"));
            out.push(opt("group", Value::Group(keys)));
            if !aggs.is_empty() {
                out.push(opt("aggregates", Value::Pairs(aggs)));
            }
            if pivot {
                out.push(opt("pivot", Value::Bool(true)));
            }
            if !(by == "key" && !desc) {
                out.push(opt("group_order", Value::Order(by, desc)));
            }
            if measure != "count" {
                out.push(opt("measure", Value::Word(measure.to_string())));
            }
        }
    }
    simple_opts(
        o,
        ty,
        &[by_key("grouped"), by_key("board")],
        prefix,
        &[],
        out,
    );
    // the board, whole
    if let Some(k) = o.get(&p("kanban")).and_then(J::as_object) {
        let strs = |x: Option<&J>| -> Option<Vec<String>> {
            x?.as_array()?
                .iter()
                .map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        let opt_str = |x: Option<&J>| -> Option<Option<String>> {
            match x {
                None | Some(J::Null) => Some(None),
                Some(J::String(s)) if !s.is_empty() => Some(Some(s.clone())),
                _ => None,
            }
        };
        let known = k.keys().all(|x| {
            matches!(
                x.as_str(),
                "field" | "title" | "cards" | "hidden" | "collapsed" | "wip" | "aggregate"
            )
        });
        let field = k.get("field").and_then(J::as_str).filter(|f| !f.is_empty());
        let wip = k.get("wip").and_then(|w| simple(Vt::Pairs, w));
        if let (
            true,
            Some(field),
            Some(cards),
            Some(hidden),
            Some(collapsed),
            Some(Value::Pairs(wip)),
            Some(title),
            Some(agg),
        ) = (
            known,
            field,
            strs(k.get("cards")),
            strs(k.get("hidden")),
            strs(k.get("collapsed")),
            wip,
            opt_str(k.get("title")),
            opt_str(k.get("aggregate")),
        ) {
            o.use_(&p("kanban"));
            out.push(opt("by", Value::Word(field.to_string())));
            if !cards.is_empty() {
                out.push(opt("cards", Value::Names(cards)));
            }
            if let Some(t) = title {
                out.push(opt("card_title", Value::Word(t)));
            }
            if !hidden.is_empty() {
                out.push(opt("hidden", Value::Names(hidden)));
            }
            if !collapsed.is_empty() {
                out.push(opt("collapsed", Value::Names(collapsed)));
            }
            if !wip.is_empty() {
                out.push(opt("wip", Value::Pairs(wip)));
            }
            if let Some(a) = agg {
                out.push(opt("sum", Value::Word(a)));
            }
        }
    }
    simple_opts(o, ty, &[by_key("known")], prefix, &[], out);
    if let Some(d) = o.get(&p("drill")) {
        if options::is_default(ty, "drill", d) {
            o.use_(&p("drill"));
        }
    }
    // the defaults of the other keys (`group: null`, `kanban: null`)
    for k in ["group", "kanban"] {
        if o.get(&p(k)).is_some_and(J::is_null) {
            o.use_(&p(k));
        }
    }
}

// ---- spreadsheets ------------------------------------------------------------------------------------

fn lift_tab(s: &Map<String, J>) -> Tab {
    let mut o = Obj::new(s);
    let id = match s.get("id").and_then(J::as_str) {
        // `tab fixed …` would read as a mode
        Some(i) if is_ident(i) && !matches!(i, "fixed" | "free" | "rows" | "cols") => {
            o.use_("id");
            Some(Name::new(i))
        }
        _ => None,
    };
    let name = s.get("name").and_then(J::as_str).map(str::to_string);
    if name.is_some() {
        o.use_("name");
    }
    let mode = s
        .get("mode")
        .and_then(J::as_str)
        .filter(|m| *m == "fixed" || *m == "free")
        .map(str::to_string);
    if mode.is_some() {
        o.use_("mode");
    }
    let rows = s.get("rows").and_then(J::as_u64);
    if rows.is_some() {
        o.use_("rows");
    }
    let cols = s.get("cols").and_then(J::as_u64);
    if cols.is_some() {
        o.use_("cols");
    }
    let comment = comment_of(&mut o);
    let mut lines = Vec::new();
    if let Some(cells) = s.get("cells").and_then(J::as_object) {
        let mut addrs: Vec<&String> = cells.keys().collect();
        addrs.sort_by_key(|a| {
            (
                !is_addr(a),
                if is_addr(a) { addr_key(a) } else { (0, 0) },
                a.to_string(),
            )
        });
        let mut all_typed = true;
        for a in addrs {
            match cell_line(a, &cells[a]) {
                Some(l) => {
                    lines.push(l);
                    o.use_(&format!("cells.{a}"));
                }
                None => all_typed = false,
            }
        }
        if all_typed {
            o.use_("cells");
        }
    }
    let pairs = |j: Option<&J>| -> Option<Vec<(String, J)>> {
        let m = j?.as_object()?;
        if m.values().all(J::is_number) && !m.is_empty() {
            Some(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        } else {
            None
        }
    };
    if let Some(p) = pairs(s.get("cols_w")) {
        o.use_("cols_w");
        lines.push(TabLine::Widths(p, Span::default()));
    }
    if let Some(p) = pairs(s.get("rows_h")) {
        o.use_("rows_h");
        lines.push(TabLine::Heights(p, Span::default()));
    }
    if let Some(m) = s.get("merges").and_then(J::as_array) {
        let ok = !m.is_empty()
            && m.iter().all(|x| {
                x.as_str()
                    .and_then(|x| x.split_once(':'))
                    .is_some_and(|(a, b)| is_addr(a) && is_addr(b))
            });
        if ok {
            o.use_("merges");
            lines.push(TabLine::Merge(
                m.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
                Span::default(),
            ));
        }
    }
    if let Some(f) = s.get("freeze").and_then(J::as_object) {
        let ok = f.keys().all(|k| k == "rows" || k == "cols")
            && f.values().all(|v| v.as_i64().is_some())
            && !f.is_empty();
        if ok {
            o.use_("freeze");
            lines.push(TabLine::Freeze {
                rows: f.get("rows").and_then(J::as_i64),
                cols: f.get("cols").and_then(J::as_i64),
                span: Span::default(),
            });
        }
    }
    let mut raws = o.raws(true);
    if id.is_none() {
        if let Some(i) = s.get("id") {
            raws.retain(|r| r.path != ["id"]);
            raws.insert(
                0,
                Raw {
                    meta: true,
                    path: vec!["id".into()],
                    value: i.clone(),
                    span: Span::default(),
                },
            );
        }
    }
    lines.extend(raws.into_iter().map(TabLine::Raw));
    Tab {
        comment,
        id,
        name,
        mode,
        rows,
        cols,
        lines,
        span: Span::default(),
    }
}

fn cell_line(addr: &str, c: &J) -> Option<TabLine> {
    if !is_addr(addr) {
        return None;
    }
    let m = c.as_object()?;
    if m.keys().any(|k| k != "v" && k != "s") {
        return None;
    }
    let value = match m.get("v") {
        None => None,
        Some(J::String(v)) => Some(match v.strip_prefix('=') {
            Some(f) if printable_formula(f) => CellValue::Formula(f.to_string(), Span::default()),
            _ => CellValue::Text(v.clone()),
        }),
        Some(_) => return None,
    };
    let style = match m.get("s") {
        None => Vec::new(),
        Some(s) => style_items(s.as_object()?),
    };
    Some(TabLine::Cell {
        addr: addr.to_string(),
        style,
        value,
        span: Span::default(),
    })
}

fn style_items(s: &Map<String, J>) -> Vec<StyleItem> {
    let mut out = Vec::new();
    let mut rest = Map::new();
    let flags = [
        ("b", "bold"),
        ("i", "italic"),
        ("u", "underline"),
        ("st", "strike"),
        ("wrap", "wrap"),
    ];
    let mut done: HashSet<&str> = HashSet::new();
    for (k, w) in flags {
        if s.get(k) == Some(&J::Bool(true)) {
            out.push(StyleItem::Flag(w.into()));
            done.insert(k);
        }
    }
    for (k, w) in [("fg", "color"), ("bg", "fill")] {
        if let Some(c) = s.get(k).and_then(J::as_str) {
            out.push(StyleItem::Color(w.into(), c.into()));
            done.insert(k);
        }
    }
    for (k, w) in [("ha", "align"), ("va", "valign")] {
        if let Some(a) = s.get(k).and_then(J::as_str).filter(|a| is_ident(a)) {
            out.push(StyleItem::Align(w.into(), a.into()));
            done.insert(k);
        }
    }
    if let Some(f) = s.get("fmt").and_then(J::as_object) {
        let ok = f
            .keys()
            .all(|k| matches!(k.as_str(), "kind" | "dec" | "thou" | "date"))
            && f.get("kind").and_then(J::as_str).is_some_and(is_ident)
            && f.get("dec").is_none_or(|d| d.as_i64().is_some())
            && f.get("thou").is_none_or(|t| t == true)
            && f.get("date").is_none_or(J::is_string);
        if ok {
            out.push(StyleItem::Format {
                kind: f["kind"].as_str().unwrap().into(),
                dec: f.get("dec").and_then(J::as_i64),
                thousands: f.contains_key("thou"),
                date: f.get("date").and_then(J::as_str).map(str::to_string),
            });
            done.insert("fmt");
        }
    }
    if let Some(b) = s.get("bd") {
        out.push(StyleItem::Border(b.clone()));
        done.insert("bd");
    }
    for (k, v) in s {
        if !done.contains(k.as_str()) {
            rest.insert(k.clone(), v.clone());
        }
    }
    if !rest.is_empty() || s.is_empty() {
        out.push(StyleItem::Style(J::Object(rest)));
    }
    out
}

// ---- saved views -------------------------------------------------------------------------------------

fn lift_saved_view(d: &J, warnings: &mut Vec<String>) -> Result<SavedView, String> {
    let name = d["name"].as_str().ok_or("a view has a name")?.to_string();
    let entity = d["entity"]
        .as_str()
        .filter(|e| is_ident(e))
        .ok_or("a view has an entity")?;
    let mut lines = Vec::new();
    if let Some(desc) = d["description"].as_str().filter(|s| !s.is_empty()) {
        lines.push(opt("description", Value::Text(desc.into())));
    }
    let shares = lift_shares(&d["shares"], warnings);
    if !shares.is_empty() {
        lines.push(opt("share", Value::Shares(shares)));
    }
    let ty = d["type"].as_str().unwrap_or("grid");
    if !options::VIEW_TYPES.contains(&ty) {
        return Err(format!("`type`: one of {}", options::VIEW_TYPES.join(", ")));
    }
    if ty != "grid" {
        lines.push(opt("type", Value::Word(ty.into())));
    }
    let empty = Map::new();
    let cfg = d["config"].as_object().unwrap_or(&empty);
    let mut o = Obj::new(cfg);
    for (k, v) in cfg {
        if v.is_null() {
            o.use_(k);
        }
    }
    let comment = comment_of(&mut o);
    view_body(&mut o, "", &mut lines);
    lines.extend(o.raws(false).into_iter().map(Line::Raw));
    Ok(SavedView {
        comment,
        name,
        entity: Name::new(entity),
        lines,
        span: Span::default(),
    })
}
