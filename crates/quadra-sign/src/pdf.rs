//! Just enough PDF to append an incremental update: objects, cross-reference tables
//! and streams (with object streams), and an object writer.

use std::collections::{HashMap, HashSet};

/// A PDF object. Strings and names hold their decoded bytes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    /// A real number, as written.
    Real(Vec<u8>),
    Str(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32, u16),
    /// Dictionary and raw (still encoded) data.
    Stream(Dict, Vec<u8>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Dict(pub Vec<(Vec<u8>, Obj)>);

impl Dict {
    pub fn get(&self, key: &str) -> Option<&Obj> {
        self.0
            .iter()
            .find(|(k, _)| k == key.as_bytes())
            .map(|(_, v)| v)
    }

    pub fn set(&mut self, key: &str, value: Obj) {
        match self.0.iter_mut().find(|(k, _)| k == key.as_bytes()) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.as_bytes().to_vec(), value)),
        }
    }

    pub fn name_is(&self, key: &str, value: &str) -> bool {
        matches!(self.get(key), Some(Obj::Name(n)) if n == value.as_bytes())
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            Some(Obj::Int(i)) => Some(*i),
            _ => None,
        }
    }
}

impl Obj {
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Obj::Dict(d) | Obj::Stream(d, _) => Some(d),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ lexer

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

#[derive(Debug, PartialEq)]
enum Tok {
    DictOpen,
    DictClose,
    ArrOpen,
    ArrClose,
    Name(Vec<u8>),
    Str(Vec<u8>),
    Num(Vec<u8>),
    Kw(Vec<u8>),
}

pub(crate) struct Lexer<'a> {
    d: &'a [u8],
    pub pos: usize,
    /// Added to positions in error messages (for objects inside object streams).
    base: Option<usize>,
}

impl<'a> Lexer<'a> {
    pub fn new(d: &'a [u8], pos: usize) -> Lexer<'a> {
        Lexer { d, pos, base: None }
    }

    fn err(&self, what: &str) -> String {
        match self.base {
            None => format!("{what} at byte {}", self.pos),
            Some(s) => format!("{what} in the object stream {s}"),
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.d.len() {
            let b = self.d[self.pos];
            if is_ws(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.pos < self.d.len()
                    && self.d[self.pos] != b'\n'
                    && self.d[self.pos] != b'\r'
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Result<Tok, String> {
        self.skip_ws();
        let d = self.d;
        let Some(&b) = d.get(self.pos) else {
            return Err(self.err("unexpected end of data"));
        };
        match b {
            b'<' if d.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                Ok(Tok::DictOpen)
            }
            b'>' if d.get(self.pos + 1) == Some(&b'>') => {
                self.pos += 2;
                Ok(Tok::DictClose)
            }
            b'[' => {
                self.pos += 1;
                Ok(Tok::ArrOpen)
            }
            b']' => {
                self.pos += 1;
                Ok(Tok::ArrClose)
            }
            b'/' => {
                self.pos += 1;
                let mut name = Vec::new();
                while let Some(&c) = d.get(self.pos) {
                    if is_ws(c) || is_delim(c) {
                        break;
                    }
                    if c == b'#' {
                        let h = d.get(self.pos + 1).and_then(|&x| hex_val(x));
                        let l = d.get(self.pos + 2).and_then(|&x| hex_val(x));
                        if let (Some(h), Some(l)) = (h, l) {
                            name.push(h * 16 + l);
                            self.pos += 3;
                            continue;
                        }
                    }
                    name.push(c);
                    self.pos += 1;
                }
                Ok(Tok::Name(name))
            }
            b'(' => self.literal_string(),
            b'<' => {
                let start = self.pos;
                self.pos += 1;
                let mut digits = Vec::new();
                loop {
                    let Some(&c) = d.get(self.pos) else {
                        self.pos = start;
                        return Err(self.err("unterminated hex string"));
                    };
                    self.pos += 1;
                    if c == b'>' {
                        break;
                    }
                    if is_ws(c) {
                        continue;
                    }
                    let v = hex_val(c).ok_or_else(|| self.err("bad character in hex string"))?;
                    digits.push(v);
                }
                if digits.len() % 2 == 1 {
                    digits.push(0);
                }
                Ok(Tok::Str(
                    digits.chunks(2).map(|p| p[0] * 16 + p[1]).collect(),
                ))
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => {
                let start = self.pos;
                while let Some(&c) = d.get(self.pos) {
                    if c.is_ascii_digit() || c == b'.' || c == b'+' || c == b'-' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Ok(Tok::Num(d[start..self.pos].to_vec()))
            }
            _ if is_delim(b) => Err(self.err(&format!("unexpected `{}`", b as char))),
            _ => {
                let start = self.pos;
                while let Some(&c) = d.get(self.pos) {
                    if is_ws(c) || is_delim(c) {
                        break;
                    }
                    self.pos += 1;
                }
                Ok(Tok::Kw(d[start..self.pos].to_vec()))
            }
        }
    }

    fn literal_string(&mut self) -> Result<Tok, String> {
        let d = self.d;
        let start = self.pos;
        self.pos += 1;
        let mut depth = 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = d.get(self.pos) else {
                self.pos = start;
                return Err(self.err("unterminated string"));
            };
            self.pos += 1;
            match c {
                b'\\' => {
                    let Some(&e) = d.get(self.pos) else { continue };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if d.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match d.get(self.pos) {
                                    Some(&o @ b'0'..=b'7') => {
                                        v = v * 8 + (o - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Tok::Str(out));
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }

    /// Reads one object (not `n g obj`).
    pub fn object(&mut self) -> Result<Obj, String> {
        self.object_depth(0)
    }

    fn object_depth(&mut self, depth: usize) -> Result<Obj, String> {
        if depth > 100 {
            return Err(self.err("objects nested too deep"));
        }
        let tok = self.next()?;
        self.object_from(tok, depth)
    }

    fn object_from(&mut self, tok: Tok, depth: usize) -> Result<Obj, String> {
        Ok(match tok {
            Tok::DictOpen => {
                let mut dict = Dict::default();
                loop {
                    match self.next()? {
                        Tok::DictClose => break,
                        Tok::Name(k) => {
                            let v = self.object_depth(depth + 1)?;
                            dict.0.push((k, v));
                        }
                        _ => return Err(self.err("expected a name as dictionary key")),
                    }
                }
                Obj::Dict(dict)
            }
            Tok::ArrOpen => {
                let mut items = Vec::new();
                loop {
                    let t = self.next()?;
                    if t == Tok::ArrClose {
                        break;
                    }
                    items.push(self.object_from(t, depth + 1)?);
                }
                Obj::Array(items)
            }
            Tok::Name(n) => Obj::Name(n),
            Tok::Str(s) => Obj::Str(s),
            Tok::Num(n) => {
                let Some(i) = parse_int(&n) else {
                    return Ok(Obj::Real(n));
                };
                // `n g R`?
                let save = self.pos;
                if i >= 0 {
                    if let Ok(Tok::Num(g)) = self.next() {
                        if let Some(g) = parse_int(&g) {
                            if let Ok(Tok::Kw(k)) = self.next() {
                                if k == b"R" && (0..=65535).contains(&g) && i <= u32::MAX as i64 {
                                    return Ok(Obj::Ref(i as u32, g as u16));
                                }
                            }
                        }
                    }
                }
                self.pos = save;
                Obj::Int(i)
            }
            Tok::Kw(k) => match k.as_slice() {
                b"true" => Obj::Bool(true),
                b"false" => Obj::Bool(false),
                b"null" => Obj::Null,
                _ => {
                    return Err(self.err(&format!(
                        "unexpected keyword `{}`",
                        String::from_utf8_lossy(&k)
                    )))
                }
            },
            Tok::DictClose | Tok::ArrClose => {
                return Err(self.err("unexpected end of dictionary or array"))
            }
        })
    }

    fn keyword(&mut self, kw: &str) -> Result<(), String> {
        match self.next()? {
            Tok::Kw(k) if k == kw.as_bytes() => Ok(()),
            _ => Err(self.err(&format!("expected `{kw}`"))),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        match self.next()? {
            Tok::Num(n) => parse_int(&n).ok_or_else(|| self.err("expected an integer")),
            _ => Err(self.err("expected an integer")),
        }
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_int(n: &[u8]) -> Option<i64> {
    std::str::from_utf8(n).ok()?.parse().ok()
}

// ------------------------------------------------------------------ document

#[derive(Clone, Copy, Debug)]
pub(crate) enum Entry {
    Free,
    InFile { offset: usize, gen: u16 },
    InStream { stream: u32, index: usize },
}

/// A PDF file read through its cross-reference sections.
pub(crate) struct Doc<'a> {
    pub data: &'a [u8],
    xref: HashMap<u32, Entry>,
    /// The newest trailer (or xref stream dictionary).
    pub trailer: Dict,
    /// Byte offset of the newest cross-reference section.
    pub last_xref: usize,
    /// Whether the newest section is a cross-reference stream.
    pub uses_xref_stream: bool,
}

impl<'a> Doc<'a> {
    pub fn load(data: &'a [u8]) -> Result<Doc<'a>, String> {
        if !data.starts_with(b"%PDF-") {
            return Err("the data is not a PDF file (it does not start with `%PDF-`)".into());
        }
        let tail_start = data.len().saturating_sub(4096);
        let sx = find_last(&data[tail_start..], b"startxref")
            .map(|p| p + tail_start)
            .ok_or("no `startxref` near the end of the file: the PDF is truncated or damaged")?;
        let mut lx = Lexer::new(data, sx + 9);
        let first = lx
            .int()
            .map_err(|e| format!("the `startxref` value cannot be read: {e}"))?;
        if first < 0 || first as usize >= data.len() {
            return Err(format!(
                "`startxref` at byte {sx} points to byte {first}, outside the file"
            ));
        }
        let mut doc = Doc {
            data,
            xref: HashMap::new(),
            trailer: Dict::default(),
            last_xref: first as usize,
            uses_xref_stream: false,
        };
        let mut next = Some(first as usize);
        let mut seen = HashSet::new();
        let mut newest = true;
        while let Some(off) = next {
            if !seen.insert(off) || seen.len() > 1000 {
                return Err(format!(
                    "the cross-reference sections loop back to byte {off}"
                ));
            }
            let (trailer, is_stream) = doc.read_section(off)?;
            if newest {
                doc.trailer = trailer.clone();
                doc.uses_xref_stream = is_stream;
                newest = false;
            }
            if !is_stream {
                if let Some(x) = trailer.int("XRefStm") {
                    doc.read_xref_stream(x as usize)?;
                }
            }
            next = match trailer.get("Prev") {
                Some(Obj::Int(p)) if *p >= 0 && (*p as usize) < data.len() => Some(*p as usize),
                Some(Obj::Int(p)) => {
                    return Err(format!(
                        "the cross-reference section at byte {off} has /Prev {p}, outside the file"
                    ))
                }
                _ => None,
            };
        }
        if doc.trailer.get("Encrypt").is_some() {
            return Err(
                "the PDF is encrypted: encrypted PDFs cannot be signed or verified here".into(),
            );
        }
        Ok(doc)
    }

    /// Reads the section at `off`, adding entries not already known; returns its trailer.
    fn read_section(&mut self, off: usize) -> Result<(Dict, bool), String> {
        let mut lx = Lexer::new(self.data, off);
        lx.skip_ws();
        if self.data[lx.pos..].starts_with(b"xref") {
            lx.pos += 4;
            let err =
                |e: String| format!("the cross-reference table at byte {off} cannot be read: {e}");
            loop {
                let save = lx.pos;
                match lx.next().map_err(err)? {
                    Tok::Kw(k) if k == b"trailer" => break,
                    Tok::Num(n) => {
                        let start =
                            parse_int(&n).ok_or_else(|| err(lx.err("bad subsection start")))?;
                        let count = lx.int().map_err(err)?;
                        if start < 0 || !(0..=10_000_000).contains(&count) {
                            return Err(err(lx.err("bad subsection")));
                        }
                        for i in 0..count {
                            let o = lx.int().map_err(err)?;
                            let g = lx.int().map_err(err)?;
                            let kind = match lx.next().map_err(err)? {
                                Tok::Kw(k) => k,
                                _ => return Err(err(lx.err("expected `n` or `f`"))),
                            };
                            let num = (start + i) as u32;
                            let entry = match kind.as_slice() {
                                b"n" => Entry::InFile {
                                    offset: o.max(0) as usize,
                                    gen: g.clamp(0, 65535) as u16,
                                },
                                b"f" => Entry::Free,
                                _ => return Err(err(lx.err("expected `n` or `f`"))),
                            };
                            self.xref.entry(num).or_insert(entry);
                        }
                    }
                    _ => {
                        return Err(err(format!(
                            "expected a subsection or `trailer` at byte {save}"
                        )));
                    }
                }
            }
            match lx.object() {
                Ok(Obj::Dict(d)) => Ok((d, false)),
                Ok(_) => Err(format!(
                    "the trailer after the cross-reference table at byte {off} is not a dictionary"
                )),
                Err(e) => Err(format!(
                    "the trailer after the cross-reference table at byte {off} cannot be read: {e}"
                )),
            }
        } else {
            let d = self.read_xref_stream(off)?;
            Ok((d, true))
        }
    }

    fn read_xref_stream(&mut self, off: usize) -> Result<Dict, String> {
        let err =
            |e: String| format!("the cross-reference stream at byte {off} cannot be read: {e}");
        let (_, _, obj) = self.indirect_at(off, 0).map_err(|e| {
            format!("expected a cross-reference table (`xref`) or stream at byte {off}: {e}")
        })?;
        let Obj::Stream(dict, raw) = obj else {
            return Err(err("it is not a stream".into()));
        };
        if !dict.name_is("Type", "XRef") {
            return Err(err("its /Type is not /XRef".into()));
        }
        let data = decode_stream(&dict, &raw).map_err(err)?;
        let w: Vec<usize> = match dict.get("W") {
            Some(Obj::Array(a)) if a.len() == 3 => a
                .iter()
                .map(|x| match x {
                    Obj::Int(i) if (0..=8).contains(i) => Ok(*i as usize),
                    _ => Err(err("bad /W".into())),
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(err("missing or bad /W".into())),
        };
        let size = dict
            .int("Size")
            .ok_or_else(|| err("missing /Size".into()))?;
        let index: Vec<i64> = match dict.get("Index") {
            Some(Obj::Array(a)) => a
                .iter()
                .map(|x| if let Obj::Int(i) = x { *i } else { -1 })
                .collect(),
            _ => vec![0, size],
        };
        if !index.len().is_multiple_of(2) || index.iter().any(|&i| i < 0) {
            return Err(err("bad /Index".into()));
        }
        let row = w[0] + w[1] + w[2];
        if row == 0 {
            return Err(err("bad /W".into()));
        }
        let field = |r: &[u8]| r.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64);
        let mut pos = 0;
        for pair in index.chunks(2) {
            for i in 0..pair[1] {
                let Some(r) = data.get(pos..pos + row) else {
                    return Err(err("the stream is shorter than its /Index says".into()));
                };
                pos += row;
                let t = if w[0] == 0 { 1 } else { field(&r[..w[0]]) };
                let f2 = field(&r[w[0]..w[0] + w[1]]);
                let f3 = field(&r[w[0] + w[1]..]);
                let entry = match t {
                    0 => Entry::Free,
                    1 => Entry::InFile {
                        offset: f2 as usize,
                        gen: f3.min(65535) as u16,
                    },
                    2 => Entry::InStream {
                        stream: f2 as u32,
                        index: f3 as usize,
                    },
                    _ => continue,
                };
                self.xref.entry((pair[0] + i) as u32).or_insert(entry);
            }
        }
        Ok(dict)
    }

    /// Parses `n g obj ... endobj` at `off`.
    fn indirect_at(&self, off: usize, depth: usize) -> Result<(u32, u16, Obj), String> {
        let mut lx = Lexer::new(self.data, off);
        let n = lx.int()?;
        let g = lx.int()?;
        lx.keyword("obj")?;
        let obj = lx.object()?;
        match lx.next() {
            Ok(Tok::Kw(k)) if k == b"stream" => {}
            _ => return Ok((n as u32, g as u16, obj)),
        }
        let Obj::Dict(dict) = obj else {
            return Err(lx.err("`stream` after something that is not a dictionary"));
        };
        let mut start = lx.pos;
        if self.data.get(start) == Some(&b'\r') {
            start += 1;
        }
        if self.data.get(start) == Some(&b'\n') {
            start += 1;
        }
        let len = match dict.get("Length") {
            Some(Obj::Int(l)) => Some(*l),
            Some(Obj::Ref(rn, _)) if depth < 4 => match self.get_depth(*rn, depth + 1) {
                Ok(Obj::Int(l)) => Some(l),
                _ => None,
            },
            _ => None,
        };
        let end = match len {
            Some(l)
                if l >= 0 && start + (l as usize) <= self.data.len() && {
                    let mut e = Lexer::new(self.data, start + l as usize);
                    e.skip_ws();
                    self.data[e.pos..].starts_with(b"endstream")
                } =>
            {
                start + l as usize
            }
            _ => {
                // Wrong or missing /Length: find `endstream`.
                let rel = find_first(&self.data[start..], b"endstream")
                    .ok_or_else(|| lx.err("stream without `endstream`"))?;
                let mut e = start + rel;
                if e > start && self.data[e - 1] == b'\n' {
                    e -= 1;
                }
                if e > start && self.data[e - 1] == b'\r' {
                    e -= 1;
                }
                e
            }
        };
        Ok((
            n as u32,
            g as u16,
            Obj::Stream(dict, self.data[start..end].to_vec()),
        ))
    }

    /// The object with number `n` (`Null` when it does not exist).
    pub fn get(&self, n: u32) -> Result<Obj, String> {
        self.get_depth(n, 0)
    }

    fn get_depth(&self, n: u32, depth: usize) -> Result<Obj, String> {
        match self.xref.get(&n).copied() {
            None | Some(Entry::Free) => Ok(Obj::Null),
            Some(Entry::InFile { offset, .. }) => {
                if offset >= self.data.len() {
                    return Err(format!("object {n} is at byte {offset} according to the cross-reference table, outside the file"));
                }
                let (num, _, obj) = self
                    .indirect_at(offset, depth)
                    .map_err(|e| format!("object {n} (at byte {offset}) cannot be read: {e}"))?;
                if num != n {
                    return Err(format!("the cross-reference table says object {n} is at byte {offset}, but object {num} is there"));
                }
                Ok(obj)
            }
            Some(Entry::InStream { stream, index }) => {
                if depth > 2 {
                    return Err(format!("object {n}: object streams nested too deep"));
                }
                let Obj::Stream(dict, raw) = self.get_depth(stream, depth + 1)? else {
                    return Err(format!(
                        "object {n} should be in object stream {stream}, which is not a stream"
                    ));
                };
                let err = |e: String| format!("object stream {stream} cannot be read: {e}");
                let data = decode_stream(&dict, &raw).map_err(err)?;
                let count = dict.int("N").ok_or_else(|| err("missing /N".into()))?;
                let first = dict
                    .int("First")
                    .ok_or_else(|| err("missing /First".into()))?;
                let mut lx = Lexer::new(&data, 0);
                lx.base = Some(stream as usize);
                let mut found = None;
                for i in 0..count {
                    let num = lx.int().map_err(err)?;
                    let off = lx.int().map_err(err)?;
                    if i as usize == index {
                        found = Some((num, off));
                        break;
                    }
                }
                let (num, off) = found.ok_or_else(|| err(format!("it has no entry {index}")))?;
                if num != n as i64 {
                    return Err(err(format!("entry {index} is object {num}, not {n}")));
                }
                let pos = (first + off) as usize;
                if pos >= data.len() {
                    return Err(err(format!("object {n} is outside the stream")));
                }
                let mut lx = Lexer::new(&data, pos);
                lx.base = Some(stream as usize);
                lx.object().map_err(err)
            }
        }
    }

    /// Follows a reference; other objects are returned as they are.
    pub fn resolve(&self, o: &Obj) -> Result<Obj, String> {
        match o {
            Obj::Ref(n, _) => self.get(*n),
            other => Ok(other.clone()),
        }
    }

    /// The generation number to use when writing a new version of object `n`.
    pub fn generation(&self, n: u32) -> u16 {
        match self.xref.get(&n) {
            Some(Entry::InFile { gen, .. }) => *gen,
            _ => 0,
        }
    }

    /// The trailer's /Size: the first free object number.
    pub fn size(&self) -> u32 {
        let declared = self.trailer.int("Size").unwrap_or(0).max(0) as u32;
        let max_known = self.xref.keys().max().map(|m| m + 1).unwrap_or(0);
        declared.max(max_known)
    }
}

fn find_last(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

pub(crate) fn find_first(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decodes a stream with no filter or FlateDecode, with PNG predictors.
fn decode_stream(dict: &Dict, raw: &[u8]) -> Result<Vec<u8>, String> {
    let filters: Vec<Vec<u8>> = match dict.get("Filter") {
        None => vec![],
        Some(Obj::Name(n)) => vec![n.clone()],
        Some(Obj::Array(a)) => a
            .iter()
            .filter_map(|x| {
                if let Obj::Name(n) = x {
                    Some(n.clone())
                } else {
                    None
                }
            })
            .collect(),
        _ => return Err("bad /Filter".into()),
    };
    let mut data = raw.to_vec();
    for f in &filters {
        if f != b"FlateDecode" {
            return Err(format!(
                "unsupported stream filter /{}",
                String::from_utf8_lossy(f)
            ));
        }
        data = miniz_oxide::inflate::decompress_to_vec_zlib(&data)
            .map_err(|e| format!("the compressed stream is damaged ({e:?})"))?;
    }
    let parms = match dict.get("DecodeParms") {
        Some(Obj::Dict(d)) => Some(d.clone()),
        Some(Obj::Array(a)) => a.iter().find_map(|x| {
            if let Obj::Dict(d) = x {
                Some(d.clone())
            } else {
                None
            }
        }),
        _ => None,
    };
    if let Some(p) = parms {
        let predictor = p.int("Predictor").unwrap_or(1);
        if predictor >= 10 {
            let colors = p.int("Colors").unwrap_or(1).max(1) as usize;
            let bpc = p.int("BitsPerComponent").unwrap_or(8).max(1) as usize;
            let columns = p.int("Columns").unwrap_or(1).max(1) as usize;
            data = png_unpredict(
                &data,
                (colors * bpc * columns).div_ceil(8),
                (colors * bpc).div_ceil(8),
            )?;
        } else if predictor == 2 {
            return Err("TIFF predictors are not supported".into());
        }
    }
    Ok(data)
}

fn png_unpredict(data: &[u8], row: usize, bpp: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        if chunk.len() < row + 1 {
            break;
        }
        let ft = chunk[0];
        let mut cur = chunk[1..].to_vec();
        for i in 0..row {
            let a = if i >= bpp { cur[i - bpp] as i16 } else { 0 };
            let b = prev[i] as i16;
            let c = if i >= bpp { prev[i - bpp] as i16 } else { 0 };
            let add = match ft {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => return Err(format!("bad PNG predictor type {ft}")),
            };
            cur[i] = (cur[i] as i16 + add) as u8;
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    Ok(out)
}

// ------------------------------------------------------------------ writer

pub(crate) fn write_obj(o: &Obj, out: &mut Vec<u8>) {
    match o {
        Obj::Null => out.extend_from_slice(b"null"),
        Obj::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Obj::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Obj::Real(r) => out.extend_from_slice(r),
        Obj::Str(s) => write_string(s, out),
        Obj::Name(n) => write_name(n, out),
        Obj::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                write_obj(x, out);
            }
            out.push(b']');
        }
        Obj::Dict(d) | Obj::Stream(d, _) => write_dict(d, out),
        Obj::Ref(n, g) => out.extend_from_slice(format!("{n} {g} R").as_bytes()),
    }
}

pub(crate) fn write_dict(d: &Dict, out: &mut Vec<u8>) {
    out.extend_from_slice(b"<<");
    for (k, v) in &d.0 {
        write_name(k, out);
        out.push(b' ');
        write_obj(v, out);
    }
    out.extend_from_slice(b">>");
}

fn write_name(n: &[u8], out: &mut Vec<u8>) {
    out.push(b'/');
    for &c in n {
        if !(0x21..=0x7e).contains(&c) || is_delim(c) || c == b'#' {
            out.extend_from_slice(format!("#{c:02X}").as_bytes());
        } else {
            out.push(c);
        }
    }
}

/// A literal string when the bytes are printable ASCII, a hex string otherwise.
pub(crate) fn write_string(s: &[u8], out: &mut Vec<u8>) {
    if s.iter().all(|&c| (0x20..0x7f).contains(&c)) {
        out.push(b'(');
        for &c in s {
            if matches!(c, b'(' | b')' | b'\\') {
                out.push(b'\\');
            }
            out.push(c);
        }
        out.push(b')');
    } else {
        out.push(b'<');
        for &c in s {
            out.extend_from_slice(format!("{c:02X}").as_bytes());
        }
        out.push(b'>');
    }
}

/// A PDF text string: PDFDocEncoding for ASCII, UTF-16BE with a byte order mark otherwise.
pub(crate) fn text_string(s: &str) -> Obj {
    if s.is_ascii() {
        Obj::Str(s.as_bytes().to_vec())
    } else {
        let mut v = vec![0xfe, 0xff];
        v.extend(s.encode_utf16().flat_map(|u| u.to_be_bytes()));
        Obj::Str(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_objects() {
        let src = b"<< /A 1 0 R /B [1 2.5 (x\\(y\\)) <414> /N#20m] /C true /D null >>";
        let o = Lexer::new(src, 0).object().unwrap();
        let d = o.as_dict().unwrap();
        assert_eq!(d.get("A"), Some(&Obj::Ref(1, 0)));
        let Some(Obj::Array(a)) = d.get("B") else {
            panic!()
        };
        assert_eq!(a[0], Obj::Int(1));
        assert_eq!(a[1], Obj::Real(b"2.5".to_vec()));
        assert_eq!(a[2], Obj::Str(b"x(y)".to_vec()));
        assert_eq!(a[3], Obj::Str(vec![0x41, 0x40]));
        assert_eq!(a[4], Obj::Name(b"N m".to_vec()));
        let mut out = Vec::new();
        write_obj(&o, &mut out);
        assert_eq!(Lexer::new(&out, 0).object().unwrap(), o);
    }

    #[test]
    fn png_up_predictor() {
        let data = [2, 1, 2, 2, 1, 1];
        assert_eq!(png_unpredict(&data, 2, 1).unwrap(), vec![1, 2, 2, 3]);
    }
}
