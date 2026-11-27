//! A small XML reader and Canonical XML 1.0 (inclusive, without comments).
//!
//! It handles the XML that Quadra writes and that FatturaPA uses: elements,
//! attributes, namespaces, text, the five predefined entities, character references
//! and CDATA sections. Comments are dropped (as C14N without comments does). DTDs,
//! processing instructions other than the XML declaration, and encodings other than
//! UTF-8 are refused with an error that says so.

use std::collections::BTreeMap;

pub(crate) const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Debug, Clone)]
pub(crate) struct Attr {
    pub prefix: Option<String>,
    pub local: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Element {
    pub prefix: Option<String>,
    pub local: String,
    pub attrs: Vec<Attr>,
    /// Namespace declarations on this element: prefix ("" for the default) and URI.
    pub ns_decls: Vec<(String, String)>,
    pub children: Vec<usize>,
    /// Byte offset of the start of the end tag (or of `/>` for an empty element).
    pub end_tag_start: usize,
    pub self_closing: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: Kind,
    pub parent: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct Document {
    pub nodes: Vec<Node>,
    pub root: usize,
}

impl Document {
    pub fn elem(&self, i: usize) -> &Element {
        match &self.nodes[i].kind {
            Kind::Element(e) => e,
            Kind::Text(_) => unreachable!("node {i} is not an element"),
        }
    }

    fn is_elem(&self, i: usize) -> bool {
        matches!(self.nodes[i].kind, Kind::Element(_))
    }

    /// All namespace bindings in scope at element `i` (nearest declaration wins).
    pub fn in_scope(&self, i: usize) -> BTreeMap<String, String> {
        let mut chain = Vec::new();
        let mut cur = Some(i);
        while let Some(c) = cur {
            chain.push(c);
            cur = self.nodes[c].parent;
        }
        let mut map = BTreeMap::new();
        for &c in chain.iter().rev() {
            for (p, u) in &self.elem(c).ns_decls {
                map.insert(p.clone(), u.clone());
            }
        }
        map
    }

    /// The namespace URI of element `i` ("" when it has none).
    pub fn ns_uri(&self, i: usize) -> String {
        let e = self.elem(i);
        let scope = self.in_scope(i);
        scope
            .get(e.prefix.as_deref().unwrap_or(""))
            .cloned()
            .unwrap_or_default()
    }

    pub fn is(&self, i: usize, ns: &str, local: &str) -> bool {
        self.is_elem(i) && self.elem(i).local == local && self.ns_uri(i) == ns
    }

    /// Element children of `i`.
    pub fn child_elems(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.elem(i)
            .children
            .iter()
            .copied()
            .filter(|&c| self.is_elem(c))
    }

    pub fn child(&self, i: usize, ns: &str, local: &str) -> Option<usize> {
        self.child_elems(i).find(|&c| self.is(c, ns, local))
    }

    /// Every element in document order, starting at `i`.
    pub fn descendants(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![i];
        while let Some(n) = stack.pop() {
            if !self.is_elem(n) {
                continue;
            }
            out.push(n);
            for &c in self.elem(n).children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// The text content of element `i` (all descendant text, concatenated).
    pub fn text(&self, i: usize) -> String {
        let mut s = String::new();
        let mut stack = vec![i];
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                Kind::Text(t) => s.push_str(t),
                Kind::Element(e) => {
                    for &c in e.children.iter().rev() {
                        stack.push(c);
                    }
                }
            }
        }
        s
    }

    /// An unqualified attribute of element `i`.
    pub fn attr(&self, i: usize, name: &str) -> Option<&str> {
        self.elem(i)
            .attrs
            .iter()
            .find(|a| a.prefix.is_none() && a.local == name)
            .map(|a| a.value.as_str())
    }

    /// The element whose `Id` (or `ID`, `id`) attribute is `id`.
    pub fn by_id(&self, id: &str) -> Option<usize> {
        self.descendants(self.root).into_iter().find(|&e| {
            ["Id", "ID", "id"]
                .iter()
                .any(|n| self.attr(e, n) == Some(id))
        })
    }
}

// ------------------------------------------------------------------ parser

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    nodes: Vec<Node>,
}

/// Parses an XML document.
pub(crate) fn parse(text: &str) -> Result<Document, String> {
    let mut p = Parser {
        s: text,
        b: text.as_bytes(),
        pos: 0,
        nodes: Vec::new(),
    };
    if text.starts_with('\u{feff}') {
        p.pos = 3;
    }
    p.prolog()?;
    if !p.b[p.pos..].starts_with(b"<") {
        return Err(p.err("expected the root element"));
    }
    let root = p.element(None, 0)?;
    // After the root: only whitespace and comments.
    loop {
        p.skip_ws();
        if p.pos >= p.b.len() {
            break;
        }
        if p.b[p.pos..].starts_with(b"<!--") {
            p.comment()?;
        } else if p.b[p.pos..].starts_with(b"<?") {
            return Err(p.err("processing instructions are not supported"));
        } else {
            return Err(p.err("unexpected content after the root element"));
        }
    }
    Ok(Document {
        nodes: p.nodes,
        root,
    })
}

impl<'a> Parser<'a> {
    fn err(&self, what: &str) -> String {
        let line = self.b[..self.pos.min(self.b.len())]
            .iter()
            .filter(|&&c| c == b'\n')
            .count()
            + 1;
        format!("XML error on line {line}: {what}")
    }

    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }

    fn prolog(&mut self) -> Result<(), String> {
        if self.b[self.pos..].starts_with(b"<?xml")
            && self
                .b
                .get(self.pos + 5)
                .is_some_and(|c| c.is_ascii_whitespace() || *c == b'?')
        {
            let end = self.s[self.pos..]
                .find("?>")
                .ok_or_else(|| self.err("unterminated XML declaration"))?;
            let decl = &self.s[self.pos..self.pos + end];
            if let Some(i) = decl.find("encoding") {
                let rest = &decl[i + 8..];
                let val: String = rest
                    .chars()
                    .skip_while(|c| *c == ' ' || *c == '=' || *c == '"' || *c == '\'')
                    .take_while(|c| *c != '"' && *c != '\'')
                    .collect();
                if !val.eq_ignore_ascii_case("utf-8") {
                    return Err(self.err(&format!(
                        "the encoding `{val}` is not supported: the XML must be UTF-8"
                    )));
                }
            }
            self.pos += end + 2;
        }
        loop {
            self.skip_ws();
            let rest = &self.b[self.pos..];
            if rest.starts_with(b"<!--") {
                self.comment()?;
            } else if rest.starts_with(b"<!DOCTYPE") {
                return Err(self.err("DTDs (`<!DOCTYPE`) are not supported"));
            } else if rest.starts_with(b"<?") {
                return Err(self.err(
                    "processing instructions other than the XML declaration are not supported",
                ));
            } else {
                return Ok(());
            }
        }
    }

    fn comment(&mut self) -> Result<(), String> {
        let end = self.s[self.pos + 4..]
            .find("-->")
            .ok_or_else(|| self.err("unterminated comment"))?;
        self.pos += 4 + end + 3;
        Ok(())
    }

    fn name(&mut self) -> Result<(Option<String>, String), String> {
        let start = self.pos;
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':') || c >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(self.err("expected a name"));
        }
        let n = &self.s[start..self.pos];
        Ok(match n.split_once(':') {
            Some((p, l)) if !p.is_empty() && !l.is_empty() && !l.contains(':') => {
                (Some(p.to_string()), l.to_string())
            }
            Some(_) => return Err(self.err(&format!("bad qualified name `{n}`"))),
            None => (None, n.to_string()),
        })
    }

    fn element(&mut self, parent: Option<usize>, depth: usize) -> Result<usize, String> {
        if depth > 200 {
            return Err(self.err("elements nested too deep"));
        }
        self.pos += 1; // '<'
        let (prefix, local) = self.name()?;
        let qn = qname(&prefix, &local);
        let mut attrs: Vec<Attr> = Vec::new();
        let mut ns_decls = Vec::new();
        let self_closing;
        loop {
            let had_ws = self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace();
            self.skip_ws();
            match self.b.get(self.pos) {
                Some(b'>') => {
                    self.pos += 1;
                    self_closing = false;
                    break;
                }
                Some(b'/') if self.b.get(self.pos + 1) == Some(&b'>') => {
                    self_closing = true;
                    break;
                }
                Some(_) if had_ws => {
                    let (ap, al) = self.name()?;
                    self.skip_ws();
                    if self.b.get(self.pos) != Some(&b'=') {
                        return Err(self.err("expected `=` after the attribute name"));
                    }
                    self.pos += 1;
                    self.skip_ws();
                    let value = self.attr_value()?;
                    match (&ap, al.as_str()) {
                        (None, "xmlns") => ns_decls.push((String::new(), value)),
                        (Some(p), l) if p == "xmlns" => {
                            if value.is_empty() {
                                return Err(
                                    self.err("a namespace prefix cannot be undeclared in XML 1.0")
                                );
                            }
                            ns_decls.push((l.to_string(), value))
                        }
                        _ => {
                            if attrs.iter().any(|a| a.prefix == ap && a.local == al) {
                                return Err(
                                    self.err(&format!("duplicate attribute `{}`", qname(&ap, &al)))
                                );
                            }
                            attrs.push(Attr {
                                prefix: ap,
                                local: al,
                                value,
                            })
                        }
                    }
                }
                _ => return Err(self.err(&format!("malformed start tag `<{qn}`"))),
            }
        }
        let idx = self.nodes.len();
        self.nodes.push(Node {
            kind: Kind::Element(Element {
                prefix,
                local,
                attrs,
                ns_decls,
                children: Vec::new(),
                end_tag_start: 0,
                self_closing,
            }),
            parent,
        });
        if self_closing {
            let at = self.pos;
            self.pos += 2;
            self.set_end(idx, at);
            return Ok(idx);
        }
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            if self.pos >= self.b.len() {
                return Err(self.err(&format!("the element `{qn}` is not closed")));
            }
            let rest = &self.b[self.pos..];
            if rest.starts_with(b"</") {
                let at = self.pos;
                self.pos += 2;
                let (p, l) = self.name()?;
                if qname(&p, &l) != qn {
                    return Err(self.err(&format!("`</{}>` closes `<{qn}>`", qname(&p, &l))));
                }
                self.skip_ws();
                if self.b.get(self.pos) != Some(&b'>') {
                    return Err(self.err("malformed end tag"));
                }
                self.pos += 1;
                self.flush_text(&mut text, idx, &mut children);
                self.set_end(idx, at);
                break;
            } else if rest.starts_with(b"<!--") {
                self.comment()?;
            } else if rest.starts_with(b"<![CDATA[") {
                let end = self.s[self.pos + 9..]
                    .find("]]>")
                    .ok_or_else(|| self.err("unterminated CDATA section"))?;
                text.push_str(&normalize_newlines(
                    &self.s[self.pos + 9..self.pos + 9 + end],
                ));
                self.pos += 9 + end + 3;
            } else if rest.starts_with(b"<?") {
                return Err(self.err("processing instructions are not supported"));
            } else if rest.starts_with(b"<!") {
                return Err(self.err("DTD declarations are not supported"));
            } else if rest.starts_with(b"<") {
                self.flush_text(&mut text, idx, &mut children);
                let c = self.element(Some(idx), depth + 1)?;
                children.push(c);
            } else {
                let end = self.s[self.pos..]
                    .find('<')
                    .map(|e| self.pos + e)
                    .unwrap_or(self.b.len());
                let raw = &self.s[self.pos..end];
                if raw.contains("]]>") {
                    return Err(self.err("`]]>` is not allowed in text"));
                }
                text.push_str(&self.decode(raw, false)?);
                self.pos = end;
            }
        }
        if let Kind::Element(e) = &mut self.nodes[idx].kind {
            e.children = children;
        }
        Ok(idx)
    }

    fn set_end(&mut self, idx: usize, at: usize) {
        if let Kind::Element(e) = &mut self.nodes[idx].kind {
            e.end_tag_start = at;
        }
    }

    fn flush_text(&mut self, text: &mut String, parent: usize, children: &mut Vec<usize>) {
        if !text.is_empty() {
            self.nodes.push(Node {
                kind: Kind::Text(std::mem::take(text)),
                parent: Some(parent),
            });
            children.push(self.nodes.len() - 1);
        }
    }

    fn attr_value(&mut self) -> Result<String, String> {
        let q = *self
            .b
            .get(self.pos)
            .ok_or_else(|| self.err("expected an attribute value"))?;
        if q != b'"' && q != b'\'' {
            return Err(self.err("attribute values must be quoted"));
        }
        let end = self.s[self.pos + 1..]
            .find(q as char)
            .ok_or_else(|| self.err("unterminated attribute value"))?;
        let raw = &self.s[self.pos + 1..self.pos + 1 + end];
        if raw.contains('<') {
            return Err(self.err("`<` is not allowed in attribute values"));
        }
        self.pos += end + 2;
        self.decode(raw, true)
    }

    /// Line-end normalization, entity decoding and (for attributes) whitespace
    /// normalization of literal tabs and newlines.
    fn decode(&self, raw: &str, attribute: bool) -> Result<String, String> {
        let raw = normalize_newlines(raw);
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw.as_str();
        while let Some(i) = rest.find('&') {
            push_literal(&mut out, &rest[..i], attribute);
            let after = &rest[i + 1..];
            let semi = after.find(';').ok_or_else(|| self.err("`&` without `;`"))?;
            let ent = &after[..semi];
            match ent {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                _ if ent.starts_with("#x") => {
                    out.push(char_ref(u32::from_str_radix(&ent[2..], 16).ok()).ok_or_else(|| self.err(&format!("bad character reference `&{ent};`")))?)
                }
                _ if ent.starts_with('#') => {
                    out.push(char_ref(ent[1..].parse().ok()).ok_or_else(|| self.err(&format!("bad character reference `&{ent};`")))?)
                }
                _ => return Err(self.err(&format!("the entity `&{ent};` is not defined (only &amp; &lt; &gt; &quot; &apos; and character references are supported)"))),
            }
            rest = &after[semi + 1..];
        }
        push_literal(&mut out, rest, attribute);
        Ok(out)
    }
}

fn push_literal(out: &mut String, s: &str, attribute: bool) {
    if attribute {
        out.extend(
            s.chars()
                .map(|c| if c == '\t' || c == '\n' { ' ' } else { c }),
        );
    } else {
        out.push_str(s);
    }
}

fn char_ref(v: Option<u32>) -> Option<char> {
    let c = char::from_u32(v?)?;
    let ok = matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}');
    ok.then_some(c)
}

fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn qname(prefix: &Option<String>, local: &str) -> String {
    match prefix {
        Some(p) => format!("{p}:{local}"),
        None => local.to_string(),
    }
}

// ------------------------------------------------------------------ C14N 1.0

/// Canonical XML 1.0 (inclusive, without comments) of the subtree at `apex`, leaving
/// out the subtree at `exclude` (the enveloped-signature transform). With `apex` equal
/// to the root element, this is the canonical form of the whole document.
pub(crate) fn c14n(doc: &Document, apex: usize, exclude: Option<usize>) -> String {
    let mut out = String::new();
    // The apex renders every namespace in scope, and inherits xml:* attributes from
    // its ancestors (both are no-ops for the root element).
    let scope = doc.in_scope(apex);
    let mut inherited: Vec<Attr> = Vec::new();
    let mut anc = doc.nodes[apex].parent;
    while let Some(a) = anc {
        for at in &doc.elem(a).attrs {
            if at.prefix.as_deref() == Some("xml") && !inherited.iter().any(|x| x.local == at.local)
            {
                inherited.push(at.clone());
            }
        }
        anc = doc.nodes[a].parent;
    }
    render(
        doc,
        apex,
        exclude,
        &BTreeMap::new(),
        Some(scope),
        &inherited,
        &mut out,
    );
    out
}

fn render(
    doc: &Document,
    i: usize,
    exclude: Option<usize>,
    rendered: &BTreeMap<String, String>,
    scope_override: Option<BTreeMap<String, String>>,
    inherited: &[Attr],
    out: &mut String,
) {
    let e = doc.elem(i);
    let scope = match scope_override {
        Some(s) => s,
        None => {
            let mut s = rendered.clone();
            for (p, u) in &e.ns_decls {
                s.insert(p.clone(), u.clone());
            }
            s
        }
    };
    let name = qname(&e.prefix, &e.local);
    out.push('<');
    out.push_str(&name);
    // Namespace declarations, sorted by prefix (the default namespace first).
    for (p, u) in &scope {
        if p == "xml" {
            continue;
        }
        let parent_has = rendered.get(p);
        let emit = if p.is_empty() && u.is_empty() {
            parent_has.is_some_and(|x| !x.is_empty())
        } else {
            parent_has != Some(u)
        };
        if emit {
            if p.is_empty() {
                out.push_str(" xmlns=\"");
            } else {
                out.push_str(" xmlns:");
                out.push_str(p);
                out.push_str("=\"");
            }
            escape_attr(u, out);
            out.push('"');
        }
    }
    // Attributes, sorted by namespace URI then local name.
    let mut attrs: Vec<(String, &Attr)> = e
        .attrs
        .iter()
        .chain(inherited.iter().filter(|x| {
            !e.attrs
                .iter()
                .any(|a| a.prefix.as_deref() == Some("xml") && a.local == x.local)
        }))
        .map(|a| {
            let uri = match a.prefix.as_deref() {
                None => String::new(),
                Some("xml") => XML_NS.to_string(),
                Some(p) => scope.get(p).cloned().unwrap_or_default(),
            };
            (uri, a)
        })
        .collect();
    attrs.sort_by(|(ua, a), (ub, b)| ua.cmp(ub).then_with(|| a.local.cmp(&b.local)));
    for (_, a) in attrs {
        out.push(' ');
        out.push_str(&qname(&a.prefix, &a.local));
        out.push_str("=\"");
        escape_attr(&a.value, out);
        out.push('"');
    }
    out.push('>');
    for &c in &e.children {
        if Some(c) == exclude {
            continue;
        }
        match &doc.nodes[c].kind {
            Kind::Text(t) => escape_text(t, out),
            Kind::Element(_) => render(doc, c, exclude, &scope, None, &[], out),
        }
    }
    out.push_str("</");
    out.push_str(&name);
    out.push('>');
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
}

/// Escapes text for writing XML (not canonical: just safe).
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::new();
    escape_text(s, &mut out);
    out.replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        let d = parse(s).unwrap();
        c14n(&d, d.root, None)
    }

    #[test]
    fn c14n_basics() {
        // Examples in the spirit of the C14N 1.0 specification, section 3.
        assert_eq!(canon("<?xml version=\"1.0\"?>\n<doc/>\n"), "<doc></doc>");
        assert_eq!(
            canon("<doc xmlns:b=\"http://b\" xmlns:a=\"http://a\" b:y=\"1\" a:z=\"2\" x=\"3\"><e xmlns:a=\"http://a\"/></doc>"),
            "<doc xmlns:a=\"http://a\" xmlns:b=\"http://b\" x=\"3\" a:z=\"2\" b:y=\"1\"><e></e></doc>"
        );
        assert_eq!(
            canon("<a xmlns=\"http://d\"><b xmlns=\"\"><c xmlns=\"\"/></b></a>"),
            "<a xmlns=\"http://d\"><b xmlns=\"\"><c></c></b></a>"
        );
        assert_eq!(
            canon("<a t='x&#9;y\n z' q=\"&quot;&lt;&gt;&amp;\">1 &lt; 2 &amp;&amp; 3 &gt; 2&#13;<![CDATA[<x>]]><!-- no --></a>"),
            "<a q=\"&quot;&lt;>&amp;\" t=\"x&#x9;y  z\">1 &lt; 2 &amp;&amp; 3 &gt; 2&#xD;&lt;x&gt;</a>"
        );
        assert_eq!(canon("<a>\r\nx\ry</a>"), "<a>\nx\ny</a>");
    }

    #[test]
    fn subset_renders_namespaces_in_scope() {
        let d = parse(
            "<r xmlns:p=\"urn:p\" xmlns=\"urn:d\"><p:a xml:lang=\"it\"><b x=\"1\"/></p:a></r>",
        )
        .unwrap();
        let a = d.child_elems(d.root).next().unwrap();
        let b = d.child_elems(a).next().unwrap();
        assert_eq!(
            c14n(&d, b, None),
            "<b xmlns=\"urn:d\" xmlns:p=\"urn:p\" x=\"1\" xml:lang=\"it\"></b>"
        );
    }

    #[test]
    fn refuses_what_it_does_not_support() {
        assert!(parse("<!DOCTYPE a><a/>").unwrap_err().contains("DTD"));
        assert!(parse("<a><?pi x?></a>")
            .unwrap_err()
            .contains("processing instructions"));
        assert!(parse("<a>&nbsp;</a>").unwrap_err().contains("not defined"));
        assert!(parse("<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a/>")
            .unwrap_err()
            .contains("UTF-8"));
    }
}
