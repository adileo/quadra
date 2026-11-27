//! XML: `record Invoice from xml:`, its fields `at "cac:Party/cbc:Name"` (elements separated
//! by `/`), `at "Amount/@currency"` (an attribute), `at "."` (the text of the record's own
//! element); without `at`, the child element of the field's name. The prefixes are the
//! record's `ns cbc "urn:…"` lines (`ns "urn:…"` is the default namespace of its unprefixed
//! elements); each record declares the prefixes of its own paths.
//!
//! The rules, read and written alike:
//!
//! - Elements and attributes are matched by namespace and local name, not by prefix: a
//!   document may use other prefixes for the same namespaces.
//! - A value is the text of an element (its text and CDATA, without its child elements) or an
//!   attribute. A record inside is an element; a list is every element the path reaches
//!   (`at "cac:InvoiceLine"`), in order. An element or attribute that is missing is none for
//!   an optional field and an error for the others; an empty element is none for an optional
//!   field that is not text.
//! - When written, the steps of a path are shared by the fields (`a/b` and `a/c` are in the
//!   same `a`), the last element of a list is repeated, an optional without a value is left
//!   out, and the namespaces are declared on the root (those of the root record) or where an
//!   element needs one.
//!
//! The document is read without a DTD: `<!DOCTYPE …>` is refused, so no entity is ever
//! defined or expanded (no external entities, no "billion laughs"); only the five predefined
//! entities and character references are. A document is at most `MAX_BYTES` long and
//! `MAX_DEPTH` elements deep, with at most `MAX_ELEMENTS` elements.

use super::{field_value, new_record, options, read_scalar, record, write_scalar};
use crate::value::V;
use quadra_ir::formats::{fields, kind, prefix, xml_path, Kind, Step};
use quadra_ir::{Entity, Model};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;
use std::rc::Rc;

/// The longest document read: 10 MiB.
pub const MAX_BYTES: usize = 10 << 20;
/// The deepest nesting of elements read.
pub const MAX_DEPTH: usize = 64;
/// The most elements a document read may have.
pub const MAX_ELEMENTS: usize = 200_000;

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// An element read: its namespace and local name, its attributes, children and text.
#[derive(Debug, Default)]
struct El {
    ns: Option<String>,
    local: String,
    /// namespace, local name, value
    attrs: Vec<(Option<String>, String, String)>,
    children: Vec<El>,
    text: String,
}

fn line_of(text: &str, pos: u64) -> usize {
    let pos = (pos as usize).min(text.len());
    text.as_bytes()[..pos]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn owned_ns(r: ResolveResult) -> Result<Option<String>, String> {
    match r {
        ResolveResult::Bound(n) => Ok(Some(n.as_ref().to_string())),
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Unknown(p) => Err(format!("the prefix `{p}` is not declared")),
    }
}

/// Reads the elements of a document, within the limits and without a DTD.
fn read_document(text: &str) -> Result<El, String> {
    if text.len() > MAX_BYTES {
        return Err(format!(
            "the document is {} bytes, more than the {} read",
            text.len(),
            MAX_BYTES
        ));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut r = NsReader::from_str(text);
    let mut stack: Vec<El> = Vec::new();
    let mut root: Option<El> = None;
    let mut count = 0usize;
    loop {
        let pos = r.buffer_position();
        let at = |p: u64| format!("line {}", line_of(text, p));
        let (ns, ev) = match r.read_resolved_event() {
            Ok((ns, ev)) => (owned_ns(ns).map_err(|e| format!("{}: {e}", at(pos)))?, ev),
            Err(e) => return Err(format!("{}: not XML: {e}", at(r.error_position()))),
        };
        let top = |stack: &mut Vec<El>, what: &str| -> Result<(), String> {
            if stack.is_empty() {
                Err(format!("{}: {what} outside the root element", at(pos)))
            } else {
                Ok(())
            }
        };
        let empty = matches!(ev, Event::Empty(_));
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                if root.is_some() {
                    return Err(format!("{}: a second root element", at(pos)));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(format!(
                        "{}: elements nested deeper than {MAX_DEPTH} levels",
                        at(pos)
                    ));
                }
                count += 1;
                if count > MAX_ELEMENTS {
                    return Err(format!("{}: more than {MAX_ELEMENTS} elements", at(pos)));
                }
                let mut el = El {
                    ns,
                    local: e.local_name().as_ref().to_string(),
                    ..El::default()
                };
                for a in e.attributes() {
                    let a = a.map_err(|e| format!("{}: not XML: {e}", at(pos)))?;
                    let key = a.key.as_ref();
                    if key == "xmlns" || key.starts_with("xmlns:") {
                        continue;
                    }
                    let (ans, local) = r.resolver().resolve_attribute(a.key);
                    let ans = owned_ns(ans).map_err(|e| format!("{}: {e}", at(pos)))?;
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| format!("{}: {e}", at(pos)))?;
                    el.attrs
                        .push((ans, local.as_ref().to_string(), value.into_owned()));
                }
                if empty {
                    match stack.last_mut() {
                        Some(p) => p.children.push(el),
                        None => root = Some(el),
                    }
                } else {
                    stack.push(el);
                }
            }
            Event::End(_) => {
                let el = stack
                    .pop()
                    .ok_or_else(|| format!("{}: an end without a start", at(pos)))?;
                match stack.last_mut() {
                    Some(p) => p.children.push(el),
                    None => root = Some(el),
                }
            }
            Event::Text(t) => {
                let t = t.xml10_content();
                if stack.is_empty() {
                    if !t.trim().is_empty() {
                        return Err(format!("{}: text outside the root element", at(pos)));
                    }
                } else if let Some(el) = stack.last_mut() {
                    el.text.push_str(&t);
                }
            }
            Event::CData(c) => {
                top(&mut stack, "CDATA")?;
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&c.into_inner());
                }
            }
            Event::GeneralRef(g) => {
                top(&mut stack, "an entity")?;
                let s: String = if g.is_char_ref() {
                    match g.resolve_char_ref() {
                        Ok(Some(c)) => c.to_string(),
                        _ => {
                            return Err(format!(
                                "{}: a character reference that is not a character",
                                at(pos)
                            ))
                        }
                    }
                } else {
                    let name = g.into_inner();
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(v) => v.to_string(),
                        None => return Err(format!("{}: `&{name};` is not an entity of XML (only `&amp;` `&lt;` `&gt;` `&quot;` `&apos;` and character references: a DTD is refused)", at(pos))),
                    }
                };
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&s);
                }
            }
            Event::DocType(_) => {
                return Err(format!(
                    "{}: a DTD (`<!DOCTYPE …>`) is refused: no entity is defined or expanded",
                    at(pos)
                ))
            }
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => {}
            Event::Eof => {
                if let Some(el) = stack.last() {
                    return Err(format!("the element `{}` is not closed", el.local));
                }
                return root.ok_or_else(|| "no element: an empty document".to_string());
            }
        }
    }
}

/// The namespaces of a record: its `ns` lines (`None`: the default namespace).
fn bindings(e: &Entity) -> Vec<(Option<String>, String)> {
    e.namespaces
        .iter()
        .map(|n| (n.prefix.clone(), n.uri.clone()))
        .collect()
}

/// The namespace and local name of a qualified name of a record (`element`: an unprefixed
/// name is in the default namespace; an unprefixed attribute is in none).
fn resolve(
    q: &str,
    ns: &[(Option<String>, String)],
    element: bool,
) -> Result<(Option<String>, String), String> {
    let (p, local) = match q.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None => (None, q),
    };
    let uri = match p {
        Some("xml") => Some(XML_NS.to_string()),
        Some(p) => Some(
            ns.iter()
                .find(|(x, _)| x.as_deref() == Some(p))
                .map(|(_, u)| u.clone())
                .ok_or_else(|| format!("the prefix `{p}` is not declared"))?,
        ),
        None if element => ns.iter().find(|(x, _)| x.is_none()).map(|(_, u)| u.clone()),
        None => None,
    };
    Ok((uri, local.to_string()))
}

/// Reads an XML document into a record of `rec`.
pub(super) fn parse(m: &Model, rec: &str, text: &str) -> Result<V, String> {
    let doc = read_document(text)?;
    let e = record(m, rec)?;
    if let Some(root) = options(e)?.root {
        let (ns, local) = resolve(&root, &bindings(e), true)?;
        if doc.ns != ns || doc.local != local {
            return Err(format!(
                "the root element is `{}`{}, not `{root}`",
                doc.local,
                doc.ns
                    .as_deref()
                    .map(|n| format!(" of {n}"))
                    .unwrap_or_default()
            ));
        }
    }
    read_record(m, rec, &doc, rec)
}

/// What a path reaches: elements, or the values of attributes.
enum Found<'a> {
    El(&'a El),
    Value(&'a str),
}

fn select<'a>(
    el: &'a El,
    steps: &[Step],
    ns: &[(Option<String>, String)],
) -> Result<Vec<Found<'a>>, String> {
    let mut cur = vec![el];
    for s in steps {
        match s {
            Step::Element(q) => {
                let (u, l) = resolve(q, ns, true)?;
                cur = cur
                    .into_iter()
                    .flat_map(|e| e.children.iter())
                    .filter(|c| c.ns == u && c.local == l)
                    .collect();
            }
            Step::Attribute(q) => {
                let (u, l) = resolve(q, ns, false)?;
                return Ok(cur
                    .into_iter()
                    .filter_map(|e| {
                        e.attrs
                            .iter()
                            .find(|(an, al, _)| *an == u && *al == l)
                            .map(|(_, _, v)| Found::Value(v.as_str()))
                    })
                    .collect());
            }
            Step::Text => break,
        }
    }
    Ok(cur.into_iter().map(Found::El).collect())
}

fn read_record(m: &Model, rec: &str, el: &El, at: &str) -> Result<V, String> {
    let e = record(m, rec)?;
    let ns = bindings(e);
    let mut values = Vec::new();
    for f in fields(m, e) {
        let Some(ty) = &f.ty else { continue };
        let k = kind(m, ty, "xml")?;
        let steps = xml_path(f)?;
        let here = format!("{at}/{}", f.at.as_deref().unwrap_or(&f.name));
        let found = select(el, &steps, &ns).map_err(|e| format!("{here}: {e}"))?;
        let text_of = |x: &Found| -> String {
            match x {
                Found::El(e) => e.text.clone(),
                Found::Value(v) => v.to_string(),
            }
        };
        let one = |k: &Kind, x: &Found, here: &str| -> Result<V, String> {
            match (k, x) {
                (Kind::Record(r), Found::El(e)) => read_record(m, r, e, here),
                (Kind::Record(_), Found::Value(_)) => {
                    Err(format!("{here}: an element was expected"))
                }
                (k, x) => {
                    let s = text_of(x);
                    if s.trim().is_empty() && !matches!(k, Kind::Text) {
                        return Err(format!("{here}: missing (empty)"));
                    }
                    read_scalar(m, k, &s, f, '.').map_err(|e| format!("{here}: {e}"))
                }
            }
        };
        let list = |inner: &Kind| -> Result<V, String> {
            Ok(V::List(Rc::new(
                found
                    .iter()
                    .enumerate()
                    .map(|(i, x)| one(inner, x, &format!("{here}[{}]", i + 1)))
                    .collect::<Result<Vec<_>, _>>()?,
            )))
        };
        let v = match &k {
            Kind::List(inner) => list(inner)?,
            Kind::Optional(inner) => match (&**inner, found.first()) {
                (_, None) => V::None,
                (Kind::List(i), Some(_)) => list(i)?,
                (k, Some(x))
                    if !matches!(k, Kind::Text | Kind::Record(_))
                        && text_of(x).trim().is_empty() =>
                {
                    V::None
                }
                (k, Some(x)) => one(k, x, &here)?,
            },
            k => match found.first() {
                None => return Err(format!("{here}: missing")),
                Some(x) => one(k, x, &here)?,
            },
        };
        values.push((f.name.clone(), v));
    }
    Ok(new_record(m, rec, values))
}

// ---- writing ------------------------------------------------------------------------------

/// An element being written: its qualified name as the record writes it and its namespace.
#[derive(Default)]
struct W {
    name: String,
    ns: Option<String>,
    /// qualified name, namespace, value
    attrs: Vec<(String, Option<String>, String)>,
    children: Vec<W>,
    text: String,
}

/// Writes a record `from xml` as a document: the declaration, then its root element.
pub(super) fn render(m: &Model, v: &V) -> Result<String, String> {
    let V::Rec { ent, .. } = v else {
        return Err(format!("{v} is not a record `from xml`"));
    };
    let e = record(m, ent)?;
    let ns = bindings(e);
    let name = options(e)?.root.unwrap_or_else(|| e.name.clone());
    let (uri, _) = resolve(&name, &ns, true)?;
    let mut root = W {
        name,
        ns: uri,
        ..W::default()
    };
    write_record(m, v, &mut root, ent)?;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    // the root declares the namespaces of its record
    let mut scope: Vec<(Option<String>, Option<String>)> = Vec::new();
    let mut decls = String::new();
    for (p, u) in &ns {
        declare(&mut decls, p.as_deref(), Some(u));
        scope.push((p.clone(), Some(u.clone())));
    }
    serialize(&root, &mut scope, decls, &mut out)?;
    Ok(out)
}

fn write_record(m: &Model, v: &V, into: &mut W, at: &str) -> Result<(), String> {
    let V::Rec {
        ent,
        fields: values,
    } = v
    else {
        return Err(format!("{at}: {v} is not a record"));
    };
    let e = record(m, ent)?;
    let ns = bindings(e);
    for f in fields(m, e) {
        let Some(ty) = &f.ty else { continue };
        let here = format!("{at}/{}", f.at.as_deref().unwrap_or(&f.name));
        let k = kind(m, ty, "xml")?;
        let steps = xml_path(f)?;
        let value = field_value(values, f);
        let put = |into: &mut W, k: &Kind, v: &V, repeat: bool, here: &str| -> Result<(), String> {
            let last = steps.iter().rposition(|s| matches!(s, Step::Element(_)));
            let mut cur = into;
            for (i, s) in steps.iter().enumerate() {
                match s {
                    Step::Element(q) => {
                        let (u, l) = resolve(q, &ns, true)?;
                        let found = if repeat && Some(i) == last {
                            None
                        } else {
                            cur.children
                                .iter()
                                .position(|c| c.ns == u && local(&c.name) == l)
                        };
                        let idx = match found {
                            Some(i) => i,
                            None => {
                                cur.children.push(W {
                                    name: q.clone(),
                                    ns: u,
                                    ..W::default()
                                });
                                cur.children.len() - 1
                            }
                        };
                        cur = &mut cur.children[idx];
                    }
                    Step::Attribute(q) => {
                        let (u, _) = resolve(q, &ns, false)?;
                        let t = write_scalar(k, v, f, '.').map_err(|e| format!("{here}: {e}"))?;
                        check_chars(&t).map_err(|e| format!("{here}: {e}"))?;
                        cur.attrs.retain(|(n, _, _)| n != q);
                        cur.attrs.push((q.clone(), u, t));
                    }
                    Step::Text => match k {
                        Kind::Record(_) => write_record(m, v, cur, here)?,
                        k => {
                            let t =
                                write_scalar(k, v, f, '.').map_err(|e| format!("{here}: {e}"))?;
                            check_chars(&t).map_err(|e| format!("{here}: {e}"))?;
                            cur.text = t;
                        }
                    },
                }
            }
            Ok(())
        };
        match (&k, &value) {
            (Kind::Optional(_), V::None) => {}
            (Kind::List(inner), V::List(items) | V::Set(items)) => {
                for (i, item) in items.iter().enumerate() {
                    put(into, inner, item, true, &format!("{here}[{}]", i + 1))?;
                }
            }
            (Kind::Optional(inner), v) => match (&**inner, v) {
                (Kind::List(i), V::List(items) | V::Set(items)) => {
                    for (n, item) in items.iter().enumerate() {
                        put(into, i, item, true, &format!("{here}[{}]", n + 1))?;
                    }
                }
                (k, v) => put(into, k, v, false, &here)?,
            },
            (k, v) => put(into, k, v, false, &here)?,
        }
    }
    Ok(())
}

fn local(q: &str) -> &str {
    q.split_once(':').map_or(q, |(_, l)| l)
}

/// The characters XML 1.0 cannot hold: the controls but tab and the new lines.
fn check_chars(t: &str) -> Result<(), String> {
    match t.chars().find(|c| {
        (*c < ' ' && !matches!(c, '\t' | '\n' | '\r')) || matches!(c, '\u{FFFE}' | '\u{FFFF}')
    }) {
        Some(c) => Err(format!(
            "the character U+{:04X} cannot be written in XML",
            u32::from(c)
        )),
        None => Ok(()),
    }
}

fn declare(out: &mut String, p: Option<&str>, u: Option<&String>) {
    match p {
        Some(p) => out.push_str(&format!(
            " xmlns:{p}=\"{}\"",
            escape(u.map_or("", |u| u), true)
        )),
        None => out.push_str(&format!(" xmlns=\"{}\"", escape(u.map_or("", |u| u), true))),
    }
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\t' if attr => out.push_str("&#9;"),
            '\n' if attr => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// Writes an element, declaring the namespaces its name and attributes need that are not in
/// scope (`decls`: those already declared on it).
fn serialize(
    w: &W,
    scope: &mut Vec<(Option<String>, Option<String>)>,
    mut decls: String,
    out: &mut String,
) -> Result<(), String> {
    let mark = scope.len();
    let bound = |scope: &[(Option<String>, Option<String>)], p: Option<&str>| -> Option<String> {
        scope
            .iter()
            .rev()
            .find(|(x, _)| x.as_deref() == p)
            .and_then(|(_, u)| u.clone())
    };
    let mut need: Vec<(Option<&str>, Option<&String>)> = vec![(prefix(&w.name), w.ns.as_ref())];
    for (q, u, _) in &w.attrs {
        if let Some(p) = prefix(q) {
            need.push((Some(p), u.as_ref()));
        }
    }
    for (p, u) in need {
        if p == Some("xml") {
            continue;
        }
        if bound(scope, p).as_ref() != u {
            if p.is_some() && u.is_none() {
                return Err(format!("`{}`: a prefix without a namespace", w.name));
            }
            declare(&mut decls, p, u);
            scope.push((p.map(str::to_string), u.cloned()));
        }
    }
    out.push('<');
    out.push_str(&w.name);
    out.push_str(&decls);
    for (q, _, v) in &w.attrs {
        out.push_str(&format!(" {q}=\"{}\"", escape(v, true)));
    }
    if w.children.is_empty() && w.text.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        out.push_str(&escape(&w.text, false));
        for c in &w.children {
            serialize(c, scope, String::new(), out)?;
        }
        out.push_str(&format!("</{}>", w.name));
    }
    scope.truncate(mark);
    Ok(())
}
