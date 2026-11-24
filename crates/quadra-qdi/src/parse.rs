//! The parser: the tokens of Quadra's lexer (same strings, numbers, comments, indentation and
//! line continuations as `.qd`), read line by line into the tree of ast.rs. `@` (not a token
//! of `.qd`) is read here: `@name` in expressions and sources.
//!
//! A line it cannot read is reported and skipped (with its block), and the rest is read: the
//! editor shows every problem at once.

use crate::ast::*;
use crate::options::{self, Vt};
use crate::text::{is_ident, string_value};
use crate::Diag;
use quadra_syntax::SyntaxKind as K;
use serde_json::Value as J;

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: K,
    start: u32,
    end: u32,
}

struct Comment {
    at: u32,
    text: String,
    taken: bool,
}

pub(crate) struct Parser<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    pos: usize,
    diags: Vec<Diag>,
    comments: Vec<Comment>,
    /// the names read since the start of the current line (for `Line::Opt::names`)
    captured: Vec<Name>,
}

const LAYOUT_ERRORS: &[&str] = &["unexpected indentation", "tabs are not allowed", "this line's indentation"];

pub fn parse(src: &str) -> (File, Vec<Diag>) {
    let (lexed, errors) = quadra_syntax::lexer::tokenize(src);
    let mut diags = Vec::new();
    for e in errors {
        // unexpected characters are reported where they are not allowed (`@` is; `!`, `'`
        // and `$` are inside a cell's formula); the layout is done again below
        if !e.message.starts_with("unexpected character") && !LAYOUT_ERRORS.iter().any(|m| e.message.starts_with(m)) {
            diags.push(Diag::new(e.message, Span::new(e.range.start().into(), e.range.end().into())));
        }
    }
    let raw: Vec<Tok> = lexed
        .iter()
        .filter(|t| !t.range.is_empty())
        .map(|t| {
            let (start, end) = (u32::from(t.range.start()), u32::from(t.range.end()));
            let text = &src[start as usize..end as usize];
            let kind = if t.kind == K::NEWLINE || (t.kind == K::WHITESPACE && (text == "\n" || text == "\r\n")) { K::NEWLINE } else { t.kind };
            Tok { kind, start, end }
        })
        .collect();
    let laid = layout(src, &raw, &mut diags);
    let mut toks = Vec::new();
    let mut comments = Vec::new();
    for t in laid {
        match t.kind {
            K::WHITESPACE => {}
            K::COMMENT => {
                let text = &src[t.start as usize..t.end as usize];
                let text = text.strip_prefix('#').unwrap_or(text);
                let text = text.strip_prefix(' ').unwrap_or(text);
                comments.push(Comment { at: t.start, text: text.trim_end().to_string(), taken: false });
            }
            _ => toks.push(t),
        }
    }
    let mut p = Parser { src, toks, pos: 0, diags, comments, captured: Vec::new() };
    let mut file = p.file();
    // comments after the last declaration belong to it
    let rest: Vec<String> = p.comments.iter_mut().filter(|c| !c.taken).map(|c| {
        c.taken = true;
        c.text.clone()
    }).collect();
    if !rest.is_empty() {
        if let Some(slot) = last_comment_slot(&mut file) {
            let mut all: Vec<String> = slot.take().map(|s| s.split('\n').map(str::to_string).collect()).unwrap_or_default();
            all.extend(rest);
            *slot = Some(all.join("\n"));
        }
    }
    p.diags.sort_by_key(|d| d.span.start);
    (file, p.diags)
}

fn is_binary_op(kind: K, text: &str) -> bool {
    matches!(kind, K::PLUS | K::MINUS | K::STAR | K::SLASH | K::QUESTION2 | K::EQ2 | K::NEQ | K::LT | K::GT | K::LE | K::GE | K::AND_KW | K::OR_KW) || (kind == K::IDENT && text == "implies")
}

/// The layout of Quadra's lexer (which line breaks count, `INDENT`/`DEDENT`: the syntax reference (quadra.run/reference/syntax)
/// §1), with one more rule so that a bracket left open does not swallow the rest of the file:
/// a line that starts no deeper than the line where the open bracket is (and not with a closing
/// bracket) closes what is open, with an error at the bracket.
fn layout(src: &str, raw: &[Tok], diags: &mut Vec<Diag>) -> Vec<Tok> {
    let text = |t: &Tok| &src[t.start as usize..t.end as usize];
    let mut out: Vec<Tok> = Vec::with_capacity(raw.len() + 64);
    let mut stack: Vec<usize> = vec![0];
    // the open brackets: the token, and the indentation of its line
    let mut open: Vec<(Tok, usize)> = Vec::new();
    let mut last_sig: Option<usize> = None;
    let mut pending_nl: Option<usize> = None;
    let mut last_nl: Option<usize> = None;
    let mut at_line_start = true;
    let mut indent_col = 0usize;
    let mut line_indent = 0usize;
    let mut indent_tab: Option<Span> = None;
    for (ri, tok) in raw.iter().copied().enumerate() {
        match tok.kind {
            K::NEWLINE => {
                out.push(Tok { kind: K::WHITESPACE, ..tok });
                last_nl = Some(out.len() - 1);
                if pending_nl.is_none() && open.is_empty() && last_sig.is_some() {
                    pending_nl = Some(out.len() - 1);
                }
                at_line_start = true;
                indent_col = 0;
                indent_tab = None;
            }
            K::WHITESPACE => {
                if at_line_start {
                    indent_col = text(&tok).len();
                    if text(&tok).contains('\t') {
                        indent_tab = Some(Span::new(tok.start, tok.end));
                    }
                }
                out.push(tok);
            }
            K::COMMENT => out.push(tok),
            _ => {
                if at_line_start {
                    line_indent = indent_col;
                    let closing = matches!(tok.kind, K::R_PAREN | K::R_BRACK | K::R_BRACE);
                    if let Some((first, at)) = open.first().copied() {
                        if indent_col <= at && !closing {
                            diags.push(Diag::new(format!("`{}` is not closed", text(&first)), Span::new(first.start, first.end)));
                            open.clear();
                            if pending_nl.is_none() {
                                pending_nl = last_nl;
                            }
                        }
                    }
                }
                if at_line_start && open.is_empty() {
                    match last_sig {
                        None => {
                            if indent_col > 0 {
                                diags.push(Diag::new("unexpected indentation", Span::new(tok.start, tok.end)));
                            }
                        }
                        Some(ls) => {
                            let prev = out[ls];
                            let starts_cont = match tok.kind {
                                K::DOT | K::QUESTION_DOT => true,
                                K::ELSE_KW => !matches!(raw[ri + 1..].iter().find(|t| !matches!(t.kind, K::WHITESPACE | K::COMMENT | K::NEWLINE)).map(|t| t.kind), Some(K::COLON) | Some(K::FAT_ARROW) | Some(K::IF_KW)),
                                k => is_binary_op(k, text(&tok)),
                            };
                            let continues_after = prev.kind == K::COMMA || (is_binary_op(prev.kind, text(&prev)) && !matches!(prev.kind, K::GT | K::LT | K::STAR));
                            if !(continues_after || starts_cont) {
                                if let Some(nl) = pending_nl {
                                    out[nl].kind = K::NEWLINE;
                                }
                                if let Some(tab) = indent_tab {
                                    diags.push(Diag::new("tabs are not allowed in indentation, use spaces", tab));
                                }
                                let top = *stack.last().unwrap();
                                if indent_col > top {
                                    stack.push(indent_col);
                                    out.push(Tok { kind: K::INDENT, start: tok.start, end: tok.start });
                                } else if indent_col < top {
                                    while indent_col < *stack.last().unwrap() {
                                        stack.pop();
                                        out.push(Tok { kind: K::DEDENT, start: tok.start, end: tok.start });
                                    }
                                    if indent_col != *stack.last().unwrap() {
                                        diags.push(Diag::new("this line's indentation does not match any outer block", Span::new(tok.start, tok.end)));
                                    }
                                }
                            }
                        }
                    }
                }
                out.push(tok);
                last_sig = Some(out.len() - 1);
                pending_nl = None;
                at_line_start = false;
                match tok.kind {
                    K::L_PAREN | K::L_BRACK | K::L_BRACE => open.push((tok, line_indent)),
                    K::R_PAREN | K::R_BRACK | K::R_BRACE => {
                        open.pop();
                    }
                    _ => {}
                }
            }
        }
    }
    let end = src.len() as u32;
    if let Some((first, _)) = open.first() {
        diags.push(Diag::new(format!("`{}` is not closed", text(first)), Span::new(first.start, first.end)));
    }
    if last_sig.is_some() {
        match pending_nl {
            Some(nl) => out[nl].kind = K::NEWLINE,
            None => out.push(Tok { kind: K::NEWLINE, start: end, end }),
        }
        for _ in 1..stack.len() {
            out.push(Tok { kind: K::DEDENT, start: end, end });
        }
    }
    out
}

/// The comment of the last declaration of a file.
fn last_comment_slot(file: &mut File) -> Option<&mut Option<String>> {
    match file.items.last_mut()? {
        Item::View(v) => Some(&mut v.comment),
        Item::App(a) => {
            if a.pages.is_empty() {
                if let Some(l) = a.lets.last_mut() {
                    return Some(&mut l.comment);
                }
                if let Some(f) = a.filters.last_mut() {
                    return Some(&mut f.comment);
                }
                return Some(&mut a.comment);
            }
            let page = a.pages.last_mut().unwrap();
            fn last_in(blocks: &mut [Block]) -> Option<&mut Component> {
                for b in blocks.iter_mut().rev() {
                    match b {
                        Block::Component(c) => return Some(c),
                        Block::Row(r) => {
                            if let Some(c) = last_in(&mut r.children) {
                                return Some(c);
                            }
                        }
                        Block::Column(c) => {
                            if let Some(c) = last_in(&mut c.children) {
                                return Some(c);
                            }
                        }
                        Block::Space(..) => {}
                    }
                }
                None
            }
            if last_in(&mut page.body).is_none() {
                return Some(&mut page.comment);
            }
            let comp = last_in(&mut page.body).unwrap();
            for l in comp.lines.iter_mut().rev() {
                match l {
                    Line::Tab(t) => return Some(&mut t.comment),
                    Line::Field(f) => return Some(&mut f.comment),
                    _ => {}
                }
            }
            Some(&mut comp.comment)
        }
    }
}

const COMPONENT_WORDS: &[&str] = &["sheet", "markdown", "chart", "kpi", "filters", "view", "button", "form", "component"];

fn is_word_kind(k: K) -> bool {
    k == K::IDENT || k.is_keyword()
}

impl<'a> Parser<'a> {
    // ---- tokens -------------------------------------------------------------------------------

    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).copied()
    }

    fn nth(&self, n: usize) -> Option<Tok> {
        self.toks.get(self.pos + n).copied()
    }

    fn text(&self, t: Tok) -> &'a str {
        &self.src[t.start as usize..t.end as usize]
    }

    fn kind(&self) -> Option<K> {
        self.peek().map(|t| t.kind)
    }

    fn at(&self, k: K) -> bool {
        self.kind() == Some(k)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.peek();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, k: K) -> bool {
        if self.at(k) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn here(&self) -> Span {
        match self.peek() {
            Some(t) => Span::new(t.start, t.end),
            None => Span::new(self.src.len() as u32, self.src.len() as u32),
        }
    }

    fn err(&mut self, msg: impl Into<String>, span: Span) {
        self.diags.push(Diag::new(msg, span));
    }

    /// The text of the current token, for messages.
    fn what(&self) -> String {
        match self.peek() {
            None => "the end of the file".into(),
            Some(t) if t.kind == K::NEWLINE => "the end of the line".into(),
            Some(t) if t.kind == K::INDENT => "an indented block".into(),
            Some(t) if t.kind == K::DEDENT => "the end of the block".into(),
            Some(t) => format!("`{}`", self.text(t)),
        }
    }

    fn is_word(&self, t: Tok) -> bool {
        is_word_kind(t.kind) && is_ident(self.text(t))
    }

    fn at_word(&self, w: &str) -> bool {
        self.peek().is_some_and(|t| self.is_word(t) && self.text(t) == w)
    }

    fn nth_word(&self, n: usize, w: &str) -> bool {
        self.nth(n).is_some_and(|t| self.is_word(t) && self.text(t) == w)
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn end_of_line(&self) -> bool {
        matches!(self.kind(), None | Some(K::NEWLINE | K::INDENT | K::DEDENT))
    }

    /// The comments before `at` not yet given to a declaration.
    fn comments_before(&mut self, at: u32) -> Option<String> {
        let mut out = Vec::new();
        for c in self.comments.iter_mut() {
            if !c.taken && c.at < at {
                c.taken = true;
                out.push(c.text.clone());
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out.join("\n"))
        }
    }

    // ---- lines and blocks --------------------------------------------------------------------

    /// Skip the rest of the line, and the block under it.
    fn skip_line(&mut self) {
        while !matches!(self.kind(), None | Some(K::NEWLINE)) {
            if self.at(K::INDENT) || self.at(K::DEDENT) {
                break;
            }
            self.pos += 1;
        }
        self.eat(K::NEWLINE);
        if self.at(K::INDENT) {
            self.skip_block();
        }
    }

    fn skip_block(&mut self) {
        if !self.eat(K::INDENT) {
            return;
        }
        let mut depth = 1;
        while let Some(t) = self.bump() {
            match t.kind {
                K::INDENT => depth += 1,
                K::DEDENT => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// The end of a line: anything left on it is reported. A block under a line that takes
    /// none is reported and skipped.
    fn end_line(&mut self) {
        if !self.end_of_line() {
            let s = self.here();
            let w = self.what();
            self.err(format!("unexpected {w}"), s);
            while !self.end_of_line() {
                self.pos += 1;
            }
        }
        self.eat(K::NEWLINE);
        if self.at(K::INDENT) {
            let s = self.here();
            self.err("unexpected indented block", s);
            self.skip_block();
        }
    }

    /// `:` and the block under the line, each line read by `f`; or nothing (the end of the
    /// line, no block). Returns whether there was a block.
    fn opt_block(&mut self, mut f: impl FnMut(&mut Self)) -> bool {
        if !self.eat(K::COLON) {
            self.end_line();
            return false;
        }
        if !self.end_of_line() {
            let s = self.here();
            let w = self.what();
            self.err(format!("expected the end of the line after `:`, found {w}"), s);
            while !self.end_of_line() {
                self.pos += 1;
            }
        }
        self.eat(K::NEWLINE);
        if !self.eat(K::INDENT) {
            let s = self.here();
            self.err("expected an indented block", s);
            return false;
        }
        self.block_lines(&mut f);
        true
    }

    fn block_lines(&mut self, f: &mut impl FnMut(&mut Self)) {
        loop {
            match self.kind() {
                None => return,
                Some(K::DEDENT) => {
                    self.pos += 1;
                    return;
                }
                Some(K::NEWLINE) => {
                    self.pos += 1;
                }
                Some(K::INDENT) => {
                    let s = self.here();
                    self.err("unexpected indentation", s);
                    self.skip_block();
                }
                _ => {
                    let before = self.pos;
                    f(self);
                    if self.pos == before {
                        let s = self.here();
                        let w = self.what();
                        self.err(format!("unexpected {w}"), s);
                        self.skip_line();
                    }
                }
            }
        }
    }

    /// An error at the current token, and the line skipped.
    fn fail_line(&mut self, msg: impl Into<String>) {
        let s = self.here();
        self.err(msg, s);
        self.skip_line();
    }

    // ---- small values ------------------------------------------------------------------------

    fn word(&mut self, what: &str) -> Option<Name> {
        match self.peek() {
            Some(t) if self.is_word(t) => {
                self.pos += 1;
                let n = Name { text: self.text(t).to_string(), span: Span::new(t.start, t.end) };
                self.captured.push(n.clone());
                Some(n)
            }
            _ => {
                let s = self.here();
                let w = self.what();
                self.err(format!("expected {what}, found {w}"), s);
                None
            }
        }
    }

    /// `a.b.c`
    fn path(&mut self, what: &str) -> Option<Name> {
        let first = self.word(what)?;
        let mut text = first.text;
        let mut span = first.span;
        while self.at(K::DOT) && self.nth(1).is_some_and(|t| self.is_word(t)) {
            self.pos += 1;
            let t = self.bump().unwrap();
            text.push('.');
            text.push_str(self.text(t));
            span.end = t.end;
        }
        let n = Name { text, span };
        self.captured.push(n.clone());
        Some(n)
    }

    fn string(&mut self, what: &str) -> Option<String> {
        match self.peek() {
            Some(t) if t.kind == K::STRING => {
                self.pos += 1;
                Some(self.string_of(t))
            }
            _ => {
                let s = self.here();
                let w = self.what();
                self.err(format!("expected {what} (a \"text\"), found {w}"), s);
                None
            }
        }
    }

    fn string_of(&self, t: Tok) -> String {
        let line_start = self.src[..t.start as usize].rfind('\n').map_or(0, |k| k + 1);
        let col = self.src[line_start..t.start as usize].chars().count();
        string_value(self.text(t), col)
    }

    fn opt_string(&mut self) -> Option<String> {
        if self.at(K::STRING) {
            let t = self.bump().unwrap();
            Some(self.string_of(t))
        } else {
            None
        }
    }

    fn int(&mut self, what: &str) -> Option<i64> {
        let neg = self.at(K::MINUS) && self.nth(1).is_some_and(|t| t.kind == K::NUMBER);
        if neg {
            self.pos += 1;
        }
        match self.peek() {
            Some(t) if t.kind == K::NUMBER => match self.text(t).replace('_', "").parse::<i64>() {
                Ok(n) => {
                    self.pos += 1;
                    Some(if neg { -n } else { n })
                }
                Err(_) => {
                    self.err(format!("expected {what} (a whole number)"), Span::new(t.start, t.end));
                    self.pos += 1;
                    None
                }
            },
            _ => {
                let s = self.here();
                let w = self.what();
                self.err(format!("expected {what} (a whole number), found {w}"), s);
                None
            }
        }
    }

    fn uint(&mut self, what: &str) -> Option<u32> {
        let s = self.here();
        let n = self.int(what)?;
        if !(0..=1_000_000).contains(&n) {
            self.err(format!("{what}: a whole number from 0"), s);
            return None;
        }
        Some(n as u32)
    }

    fn colon(&mut self) -> bool {
        if self.eat(K::COLON) {
            return true;
        }
        let s = self.here();
        let w = self.what();
        self.err(format!("expected `:`, found {w}"), s);
        false
    }

    /// `@name` (the `@` right before the name)
    fn at_param(&self) -> bool {
        self.peek().is_some_and(|t| t.kind == K::AT) && self.nth(1).is_some_and(|n| self.is_word(n) && n.start == self.peek().unwrap().end)
    }

    fn param(&mut self) -> Option<String> {
        if !self.at_param() {
            let s = self.here();
            let w = self.what();
            self.err(format!("expected `@name`, found {w}"), s);
            return None;
        }
        let at = self.bump().unwrap();
        let t = self.bump().unwrap();
        self.captured.push(Name { text: format!("@{}", self.text(t)), span: Span::new(at.start, t.end) });
        Some(self.text(t).to_string())
    }

    // ---- data literals -------------------------------------------------------------------------

    /// A data literal (JSON written the Quadra way).
    fn data(&mut self) -> Option<J> {
        let Some(t) = self.peek() else {
            self.err("expected a value", self.here());
            return None;
        };
        match t.kind {
            K::L_BRACK => {
                self.pos += 1;
                let mut out = Vec::new();
                while !self.at(K::R_BRACK) {
                    out.push(self.data()?);
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                if !self.eat(K::R_BRACK) {
                    let s = self.here();
                    let w = self.what();
                    self.err(format!("expected `]`, found {w}"), s);
                    return None;
                }
                Some(J::Array(out))
            }
            K::L_BRACE => {
                self.pos += 1;
                let mut out = serde_json::Map::new();
                while !self.at(K::R_BRACE) {
                    let k = match self.peek() {
                        Some(t) if t.kind == K::STRING => {
                            self.pos += 1;
                            self.string_of(t)
                        }
                        Some(t) if self.is_word(t) || t.kind == K::NUMBER => {
                            self.pos += 1;
                            self.text(t).to_string()
                        }
                        _ => {
                            let s = self.here();
                            let w = self.what();
                            self.err(format!("expected a key, found {w}"), s);
                            return None;
                        }
                    };
                    if !self.colon() {
                        return None;
                    }
                    let v = self.data()?;
                    out.insert(k, v);
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                if !self.eat(K::R_BRACE) {
                    let s = self.here();
                    let w = self.what();
                    self.err(format!("expected `}}`, found {w}"), s);
                    return None;
                }
                Some(J::Object(out))
            }
            K::STRING => {
                self.pos += 1;
                Some(J::String(self.string_of(t)))
            }
            K::MINUS | K::NUMBER => self.number(),
            K::DATE | K::DATETIME | K::YEAR_MONTH | K::PERCENT => {
                self.pos += 1;
                Some(J::String(self.text(t).to_string()))
            }
            _ if self.is_word(t) => {
                self.pos += 1;
                Some(match self.text(t) {
                    "none" | "null" => J::Null,
                    "true" => J::Bool(true),
                    "false" => J::Bool(false),
                    w => J::String(w.to_string()),
                })
            }
            _ => {
                let w = self.what();
                self.err(format!("expected a value, found {w}"), Span::new(t.start, t.end));
                None
            }
        }
    }

    /// A number (`-2`, `1.5`, `1e21`); a number followed by a unit (`5 EUR`, `9.90 EUR/pcs`)
    /// is that text.
    fn number(&mut self) -> Option<J> {
        let start = self.here();
        let neg = self.eat(K::MINUS);
        let Some(t) = self.peek().filter(|t| t.kind == K::NUMBER) else {
            let w = self.what();
            self.err(format!("expected a number, found {w}"), start);
            return None;
        };
        self.pos += 1;
        let mut text = String::new();
        if neg {
            text.push('-');
        }
        text.push_str(&self.text(t).replace('_', ""));
        let mut end = t.end;
        // an exponent, written against the number: 1e21, 1E-7
        if let Some(n) = self.peek().filter(|n| n.start == end && n.kind == K::IDENT) {
            let w = self.text(n);
            if (w.starts_with('e') || w.starts_with('E')) && w[1..].chars().all(|c| c.is_ascii_digit()) {
                if w.len() > 1 {
                    text.push_str(w);
                    end = n.end;
                    self.pos += 1;
                } else if let (Some(sign), Some(d)) = (self.nth(1), self.nth(2)) {
                    if sign.start == n.end && matches!(sign.kind, K::PLUS | K::MINUS) && d.start == sign.end && d.kind == K::NUMBER {
                        text.push_str(w);
                        text.push_str(self.text(sign));
                        text.push_str(self.text(d));
                        end = d.end;
                        self.pos += 3;
                    }
                }
            }
        }
        // a quantity, an amount, a price: kept as the text
        if let Some(u) = self.peek().filter(|u| self.is_word(*u) && !matches!(self.text(*u), "default" | "as")) {
            let mut q = format!("{} {}", text, self.text(u));
            self.pos += 1;
            if let (Some(sl), Some(u2)) = (self.peek(), self.nth(1)) {
                if sl.kind == K::SLASH && sl.start == u.end && u2.start == sl.end && self.is_word(u2) {
                    q.push('/');
                    q.push_str(self.text(u2));
                    self.pos += 2;
                }
            }
            return Some(J::String(q));
        }
        match serde_json::from_str::<serde_json::Number>(&text) {
            Ok(n) => Some(J::Number(n)),
            Err(_) => {
                self.err(format!("`{text}` is not a number"), Span::new(start.start, end));
                None
            }
        }
    }

    // ---- expressions -------------------------------------------------------------------------

    /// An expression: the rest of the line (continued on the lines after it as in `.qd`), up
    /// to a word of `stop` outside brackets.
    fn expr_until(&mut self, stop: &[&str]) -> Option<Expr> {
        let start = self.pos;
        let mut depth = 0i32;
        while !self.end_of_line() {
            let t = self.peek().unwrap();
            if depth == 0 && self.is_word(t) && stop.contains(&self.text(t)) {
                break;
            }
            match t.kind {
                K::L_PAREN | K::L_BRACK | K::L_BRACE => depth += 1,
                K::R_PAREN | K::R_BRACK | K::R_BRACE => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        let toks = &self.toks[start..self.pos];
        if toks.is_empty() {
            let s = self.here();
            let w = self.what();
            self.err(format!("expected an expression, found {w}"), s);
            return None;
        }
        let span = Span::new(toks[0].start, toks[toks.len() - 1].end);
        // raw "…": a text kept as it is
        if toks.len() == 2 && self.is_word(toks[0]) && self.text(toks[0]) == "raw" && toks[1].kind == K::STRING {
            return Some(Expr { text: self.string_of(toks[1]), raw: true, span, map: Vec::new() });
        }
        let mut text = String::new();
        let mut check = String::new();
        let mut map = Vec::new();
        for (k, t) in toks.iter().enumerate() {
            if k > 0 {
                let prev = toks[k - 1];
                let gap = &self.src[prev.end as usize..t.start as usize];
                let sep = if gap.contains('\n') {
                    if matches!(t.kind, K::DOT | K::QUESTION_DOT | K::R_PAREN | K::R_BRACK | K::R_BRACE) || matches!(prev.kind, K::L_PAREN | K::L_BRACK | K::L_BRACE) {
                        ""
                    } else {
                        " "
                    }
                } else {
                    gap
                };
                text.push_str(sep);
                check.push_str(sep);
            }
            map.push((text.len() as u32, t.start));
            let tt = self.text(*t);
            text.push_str(tt);
            check.push_str(tt);
        }
        if quadra_syntax::parse_expr(&check).is_none() {
            self.err(format!("`{text}` is not an expression (write `raw \"…\"` to keep a text that is not one)"), span);
        }
        Some(Expr { text, raw: false, span, map })
    }

    fn expr(&mut self) -> Option<Expr> {
        self.expr_until(&[])
    }

    // ---- sources -----------------------------------------------------------------------------

    /// `@name`, `id 42`, `cell pivot "B2"`, or a data literal.
    fn source(&mut self) -> Option<Source> {
        if self.at_param() {
            return self.param().map(Source::Param);
        }
        if self.at_word("id") && self.nth(1).is_some_and(|t| t.kind == K::NUMBER) {
            self.pos += 1;
            let s = self.here();
            let n = self.int("an id")?;
            if n < 0 {
                self.err("an id is a positive number", s);
                return None;
            }
            return Some(Source::Id(n as u64));
        }
        if self.at_word("cell") && self.nth(1).is_some_and(|t| self.is_word(t)) && self.nth(2).is_some_and(|t| t.kind == K::STRING) {
            self.pos += 1;
            let component = self.word("a component")?.text;
            let cell = self.string("a cell")?;
            return Some(Source::Cell { component, cell });
        }
        self.data().map(Source::Value)
    }

    /// `run: @order.confirm()`, `Order[@x].ship(carrier=Road)`, `Order.recompute()`
    fn run(&mut self) -> Option<Run> {
        let start = self.here();
        let target = if self.at_param() {
            Target::Param(self.param()?)
        } else {
            let entity = self.word("`@name` or an entity")?.text;
            if self.eat(K::L_BRACK) {
                let src = match self.source()? {
                    Source::Value(J::Number(n)) if n.as_u64().is_some() => Source::Id(n.as_u64().unwrap()),
                    s @ (Source::Param(_) | Source::Id(_) | Source::Cell { .. }) => s,
                    Source::Value(_) => {
                        self.err("the record: `@name`, an id or `cell <component> \"<cell>\"`", start);
                        return None;
                    }
                };
                if !self.eat(K::R_BRACK) {
                    let s = self.here();
                    let w = self.what();
                    self.err(format!("expected `]`, found {w}"), s);
                    return None;
                }
                Target::Of(entity, src)
            } else {
                Target::Entity(entity)
            }
        };
        if !self.eat(K::DOT) {
            let s = self.here();
            let w = self.what();
            self.err(format!("expected `.` and the action, found {w}"), s);
            return None;
        }
        let action = self.word("an action")?.text;
        if !self.eat(K::L_PAREN) {
            let s = self.here();
            let w = self.what();
            self.err(format!("expected `(`, found {w}"), s);
            return None;
        }
        let mut args = Vec::new();
        while !self.at(K::R_PAREN) {
            let name = self.word("a parameter")?.text;
            if !self.eat(K::EQ) {
                let s = self.here();
                let w = self.what();
                self.err(format!("expected `=`, found {w}"), s);
                return None;
            }
            let v = self.source()?;
            args.push((name, v));
            if !self.eat(K::COMMA) {
                break;
            }
        }
        let end = self.here();
        if !self.eat(K::R_PAREN) {
            let w = self.what();
            self.err(format!("expected `)`, found {w}"), end);
            return None;
        }
        Some(Run { target, action, args, span: Span::new(start.start, end.end) })
    }

    // ---- values of options ---------------------------------------------------------------------

    fn names(&mut self) -> Option<Vec<String>> {
        if self.at(K::L_BRACK) {
            let s = self.here();
            return match self.data()? {
                J::Array(a) if a.iter().all(J::is_string) => Some(a.into_iter().map(|x| x.as_str().unwrap().to_string()).collect()),
                _ => {
                    self.err("a list of names", s);
                    None
                }
            };
        }
        let mut out = Vec::new();
        loop {
            out.push(self.path("a name")?.text);
            if !self.eat(K::COMMA) {
                break;
            }
        }
        Some(out)
    }

    fn pairs(&mut self) -> Option<Vec<(String, J)>> {
        let mut out = Vec::new();
        loop {
            let k = match self.peek() {
                Some(t) if t.kind == K::STRING => {
                    self.pos += 1;
                    self.string_of(t)
                }
                Some(t) if t.kind == K::NUMBER => {
                    self.pos += 1;
                    self.text(t).to_string()
                }
                _ => self.path("a name")?.text,
            };
            let v = self.data()?;
            out.push((k, v));
            if !self.eat(K::COMMA) {
                break;
            }
        }
        Some(out)
    }

    fn value(&mut self, vt: Vt, key: &str) -> Option<Value> {
        Some(match vt {
            Vt::Text => Value::Text(self.string(&format!("`{key}`"))?),
            Vt::Int => Value::Int(self.int(&format!("`{key}`"))?),
            Vt::Bool => match self.peek().map(|t| self.text(t)) {
                Some("true") => {
                    self.pos += 1;
                    Value::Bool(true)
                }
                Some("false") => {
                    self.pos += 1;
                    Value::Bool(false)
                }
                _ => {
                    let s = self.here();
                    self.err(format!("`{key}`: true or false"), s);
                    return None;
                }
            },
            Vt::Word(set) => {
                let s = self.here();
                let w = if self.at(K::STRING) { self.opt_string().unwrap() } else { self.path(&format!("`{key}`"))?.text };
                if !set.is_empty() && !set.contains(&w.as_str()) {
                    self.err(format!("`{key}`: one of {}", set.join(", ")), s);
                    return None;
                }
                Value::Word(w)
            }
            Vt::Names => Value::Names(self.names()?),
            Vt::Pairs => Value::Pairs(self.pairs()?),
            Vt::Expr => Value::Expr(self.expr()?),
            Vt::Source => {
                for kind in ["cell", "range"] {
                    if self.at_word(kind) && self.nth(1).is_some_and(|t| self.is_word(t)) && self.nth(2).is_some_and(|t| t.kind == K::STRING) {
                        self.pos += 1;
                        let component = self.word("a component")?.text;
                        let r = self.string("a cell")?;
                        return Some(Value::Source(if kind == "cell" { DataSource::Cell { component, cell: r } } else { DataSource::Range { component, range: r } }));
                    }
                }
                Value::Source(DataSource::Expr(self.expr()?))
            }
            Vt::Compare => {
                if self.eat_word("previous") {
                    Value::Compare(Compare::Previous(self.param()?))
                } else if self.eat_word("target") {
                    Value::Compare(Compare::Target(self.expr()?))
                } else {
                    let s = self.here();
                    self.err("`compare`: `previous @<filter>` or `target <expression>`", s);
                    return None;
                }
            }
            Vt::Run => Value::Run(self.run()?),
            Vt::From => Value::From(self.source()?),
            Vt::Sort => {
                let mut out = Vec::new();
                loop {
                    let f = self.path("a field")?.text;
                    let desc = if self.eat_word("desc") {
                        true
                    } else {
                        self.eat_word("asc");
                        false
                    };
                    out.push((f, desc));
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                Value::Sort(out)
            }
            Vt::Group => {
                let mut out = Vec::new();
                loop {
                    let f = self.path("a field")?.text;
                    let bucket = if self.eat_word("by") { Some(self.word("a bucket (day, week, month, quarter, year)")?.text) } else { None };
                    out.push((f, bucket));
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                Value::Group(out)
            }
            Vt::Order => {
                let by = if self.at(K::STRING) { self.opt_string().unwrap() } else { self.path("key, count or an aggregate")?.text };
                Value::Order(by, self.eat_word("desc"))
            }
            Vt::Shares => Value::Shares(self.shares()?),
            Vt::Flags(set) => {
                let mut out = Vec::new();
                loop {
                    let s = self.here();
                    let w = self.word(&format!("one of {}", set.join(", ")))?.text;
                    if !set.contains(&w.as_str()) {
                        self.err(format!("`{key}`: some of {}", set.join(", ")), s);
                        return None;
                    }
                    out.push(w);
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                Value::Flags(out)
            }
            Vt::After => {
                let s = self.here();
                if self.eat_word("select") {
                    let entity = self.word("the entity of the record")?.text;
                    if !self.eat(K::AS_KW) {
                        let s = self.here();
                        self.err("expected `as <name>`", s);
                        return None;
                    }
                    let name = self.word("a name")?.text;
                    Value::After(After::Select { entity, name })
                } else {
                    let w = self.word("clear, keep, open or select")?.text;
                    Value::After(match w.as_str() {
                        "clear" => After::Clear,
                        "keep" => After::Keep,
                        "open" => After::Open,
                        _ => {
                            self.err("`after`: clear, keep, open or `select <Entity> as <name>`", s);
                            return None;
                        }
                    })
                }
            }
        })
    }

    fn shares(&mut self) -> Option<Vec<Share>> {
        let mut out = Vec::new();
        loop {
            let role = if self.eat_word("everyone") {
                None
            } else if self.eat_word("role") {
                Some(self.word("a role")?.text)
            } else {
                let s = self.here();
                self.err("a share: `role <Role> use|change` or `everyone use|change`", s);
                return None;
            };
            let change = if self.eat_word("change") {
                true
            } else if self.eat_word("use") {
                false
            } else {
                let s = self.here();
                self.err("expected `use` or `change`", s);
                return None;
            };
            out.push(Share { role, change });
            if !self.eat(K::COMMA) {
                break;
            }
        }
        Some(out)
    }

    /// `config a.b: <data>` / `meta a.b: <data>`
    fn raw_line(&mut self) -> Option<Raw> {
        let start = self.here();
        let meta = self.at_word("meta");
        self.pos += 1;
        let mut path = Vec::new();
        loop {
            match self.peek() {
                Some(t) if t.kind == K::STRING => {
                    self.pos += 1;
                    path.push(self.string_of(t));
                }
                Some(t) if self.is_word(t) => {
                    self.pos += 1;
                    path.push(self.text(t).to_string());
                }
                _ => {
                    let s = self.here();
                    let w = self.what();
                    self.err(format!("expected a key, found {w}"), s);
                    return None;
                }
            }
            if !self.eat(K::DOT) {
                break;
            }
        }
        if !self.colon() {
            return None;
        }
        let value = self.data()?;
        let end = self.toks[self.pos - 1].end;
        Some(Raw { meta, path, value, span: Span::new(start.start, end) })
    }

    fn at_raw(&self) -> bool {
        (self.at_word("config") || self.at_word("meta")) && self.nth(1).is_some_and(|t| self.is_word(t) || t.kind == K::STRING)
    }

    // ---- the file ------------------------------------------------------------------------------

    fn file(&mut self) -> File {
        let mut items = Vec::new();
        loop {
            while self.eat(K::NEWLINE) {}
            let Some(t) = self.peek() else { break };
            if t.kind == K::INDENT {
                self.err("unexpected indentation", Span::new(t.start, t.end));
                self.skip_block();
                continue;
            }
            if t.kind == K::DEDENT {
                self.pos += 1;
                continue;
            }
            let comment = self.comments_before(t.start);
            if self.at_word("app") {
                if let Some(a) = self.app(comment) {
                    items.push(Item::App(a));
                }
            } else if self.at_word("view") {
                if let Some(v) = self.saved_view(comment) {
                    items.push(Item::View(v));
                }
            } else {
                self.fail_line(format!("expected `app` or `view`, found {}", self.what()));
            }
        }
        File { items }
    }

    fn app(&mut self, comment: Option<String>) -> Option<App> {
        let start = self.bump().unwrap().start;
        let name = match self.string("the app's name") {
            Some(n) => n,
            None => {
                self.skip_line();
                return None;
            }
        };
        let mut app = App { comment, name, description: None, shares: Vec::new(), filters: Vec::new(), lets: Vec::new(), pages: Vec::new(), raws: Vec::new(), span: Span::default() };
        self.opt_block(|p| {
            let t = p.peek().unwrap();
            if p.at_word("description") && p.nth(1).is_some_and(|t| t.kind == K::COLON) {
                p.pos += 2;
                if let Some(s) = p.string("the description") {
                    app.description = Some(s);
                }
                p.end_line();
            } else if p.at_word("share") && p.nth(1).is_some_and(|t| t.kind == K::COLON) {
                p.pos += 2;
                if let Some(s) = p.shares() {
                    app.shares.extend(s);
                }
                p.end_line();
            } else if p.at_word("filter") {
                let comment = p.comments_before(t.start);
                if let Some(f) = p.filter_decl(comment) {
                    app.filters.push(f);
                }
            } else if p.at(K::LET_KW) {
                let comment = p.comments_before(t.start);
                p.pos += 1;
                let Some(name) = p.word("a name") else { return p.skip_line() };
                if !p.eat(K::EQ) {
                    return p.fail_line(format!("expected `=`, found {}", p.what()));
                }
                let Some(expr) = p.expr() else { return p.skip_line() };
                let end = expr.span.end;
                app.lets.push(Let { comment, name, expr, span: Span::new(t.start, end) });
                p.end_line();
            } else if p.at_word("page") {
                let comment = p.comments_before(t.start);
                if let Some(pg) = p.page(comment) {
                    app.pages.push(pg);
                }
            } else if p.at_raw() && p.at_word("meta") {
                if let Some(r) = p.raw_line() {
                    app.raws.push(r);
                }
                p.end_line();
            } else {
                p.fail_line(format!("expected `description`, `share`, `filter`, `let`, `page` or `meta`, found {}", p.what()));
            }
        });
        app.span = Span::new(start, self.toks.get(self.pos.saturating_sub(1)).map_or(start, |t| t.end));
        Some(app)
    }

    fn filter_decl(&mut self, comment: Option<String>) -> Option<FilterDecl> {
        let start = self.bump().unwrap().start;
        let Some(name) = self.word("the filter's name") else {
            self.skip_line();
            return None;
        };
        let label = self.opt_string();
        if !self.colon() {
            self.skip_line();
            return None;
        }
        let s = self.here();
        let Some(widget) = self.word("a widget") else {
            self.skip_line();
            return None;
        };
        if !options::WIDGETS.contains(&widget.text.as_str()) {
            self.err(format!("a widget: {}", options::WIDGETS.join(", ")), s);
        }
        let Some(path) = self.path("the entity and field it filters (`Order.customer.region`)") else {
            self.skip_line();
            return None;
        };
        let (entity, field) = match path.text.split_once('.') {
            Some((e, f)) => {
                let es = Span::new(path.span.start, path.span.start + e.len() as u32);
                let fs = Span::new(es.end + 1, path.span.end);
                (Name { text: e.into(), span: es }, Name { text: f.into(), span: fs })
            }
            None => {
                self.err("the entity and the field it filters: `Order.customer.region`", path.span);
                (path.clone(), Name::default())
            }
        };
        let default = if self.eat_word("default") { self.data() } else { None };
        let end = self.toks[self.pos - 1].end;
        let mut f = FilterDecl { comment, name, label, widget, entity, field, default, raws: Vec::new(), span: Span::new(start, end) };
        self.end_line_or_meta(&mut f.raws);
        Some(f)
    }

    /// The end of a line under which a block of `meta` lines may come.
    fn end_line_or_meta(&mut self, raws: &mut Vec<Raw>) {
        if !self.end_of_line() {
            let s = self.here();
            let w = self.what();
            self.err(format!("unexpected {w}"), s);
            while !self.end_of_line() {
                self.pos += 1;
            }
        }
        self.eat(K::NEWLINE);
        if self.eat(K::INDENT) {
            self.block_lines(&mut |p: &mut Self| {
                if p.at_raw() && p.at_word("meta") {
                    if let Some(r) = p.raw_line() {
                        raws.push(r);
                    }
                    p.end_line();
                } else {
                    p.fail_line(format!("expected `meta`, found {}", p.what()));
                }
            });
        }
    }

    fn page(&mut self, comment: Option<String>) -> Option<Page> {
        let start = self.bump().unwrap().start;
        let id = if self.peek().is_some_and(|t| self.is_word(t)) { self.word("a name") } else { None };
        let name = self.opt_string();
        let mut page = Page { comment, id, name, body: Vec::new(), raws: Vec::new(), span: Span::default() };
        self.opt_block(|p| {
            if p.at_word("row") {
                if let Some(r) = p.row() {
                    page.body.push(Block::Row(r));
                }
            } else if p.at_word("space") {
                if let Some(b) = p.space() {
                    page.body.push(b);
                }
            } else if p.at_raw() && p.at_word("meta") {
                if let Some(r) = p.raw_line() {
                    page.raws.push(r);
                }
                p.end_line();
            } else if p.peek().is_some_and(|t| p.is_word(t) && COMPONENT_WORDS.contains(&p.text(t))) {
                if let Some(c) = p.component() {
                    if c.at.is_none() {
                        p.err("a component of a page is in a `row`, or placed with `at x, y size w x h`", c.span);
                    }
                    page.body.push(Block::Component(Box::new(c)));
                }
            } else {
                p.fail_line(format!("expected `row`, `space`, a component placed with `at`, or `meta`, found {}", p.what()));
            }
        });
        page.span = Span::new(start, self.toks.get(self.pos.saturating_sub(1)).map_or(start, |t| t.end));
        Some(page)
    }

    fn space(&mut self) -> Option<Block> {
        let s = self.bump().unwrap();
        let n = self.uint("the empty space")?;
        let end = self.toks[self.pos - 1].end;
        self.end_line();
        Some(Block::Space(n, Span::new(s.start, end)))
    }

    fn row(&mut self) -> Option<Row> {
        let start = self.bump().unwrap().start;
        let height = if self.eat_word("height") { Some(self.uint("the height")?) } else { None };
        let mut row = Row { height, children: Vec::new(), span: Span::default() };
        let had = self.opt_block(|p| {
            if p.at_word("column") {
                if let Some(c) = p.column() {
                    row.children.push(Block::Column(c));
                }
            } else if p.at_word("space") {
                if let Some(b) = p.space() {
                    row.children.push(b);
                }
            } else if p.peek().is_some_and(|t| p.is_word(t) && COMPONENT_WORDS.contains(&p.text(t))) {
                if let Some(c) = p.component() {
                    if c.at.is_some() {
                        p.err("`at` places a component on the page, not in a row", c.span);
                    }
                    row.children.push(Block::Component(Box::new(c)));
                }
            } else {
                p.fail_line(format!("expected a component, `column` or `space`, found {}", p.what()));
            }
        });
        if !had {
            self.err("a row holds components", Span::new(start, start + 3));
        }
        row.span = Span::new(start, self.toks.get(self.pos.saturating_sub(1)).map_or(start, |t| t.end));
        Some(row)
    }

    fn column(&mut self) -> Option<Column> {
        let start = self.bump().unwrap().start;
        let width = if self.eat_word("width") { Some(self.uint("the width")?) } else { None };
        let mut col = Column { width, children: Vec::new(), span: Span::default() };
        let had = self.opt_block(|p| {
            if p.at_word("row") {
                if let Some(r) = p.row() {
                    col.children.push(Block::Row(r));
                }
            } else if p.at_word("space") {
                if let Some(b) = p.space() {
                    col.children.push(b);
                }
            } else if p.peek().is_some_and(|t| p.is_word(t) && COMPONENT_WORDS.contains(&p.text(t))) {
                if let Some(c) = p.component() {
                    if c.at.is_some() {
                        p.err("`at` places a component on the page, not in a column", c.span);
                    }
                    col.children.push(Block::Component(Box::new(c)));
                }
            } else {
                p.fail_line(format!("expected a component, `row` or `space`, found {}", p.what()));
            }
        });
        if !had {
            self.err("a column holds components", Span::new(start, start + 6));
        }
        col.span = Span::new(start, self.toks.get(self.pos.saturating_sub(1)).map_or(start, |t| t.end));
        Some(col)
    }

    // ---- components ----------------------------------------------------------------------------

    fn component(&mut self) -> Option<Component> {
        let first = self.peek().unwrap();
        let comment = self.comments_before(first.start);
        self.pos += 1;
        let mut kind = self.text(first).to_string();
        if kind == "component" {
            match self.word("the component's type") {
                Some(t) => kind = t.text,
                None => {
                    self.skip_line();
                    return None;
                }
            }
        } else {
            kind = options::json_type(&kind).to_string();
        }
        // a name, unless the word is `width`/`height`/`at` and a number follows
        let name = match (self.peek(), self.nth(1)) {
            (Some(t), next) if self.is_word(t) => {
                let w = self.text(t);
                let option_word = matches!(w, "width" | "height" | "at") && next.is_some_and(|n| n.kind == K::NUMBER);
                if option_word {
                    None
                } else {
                    self.word("a name")
                }
            }
            _ => None,
        };
        let title = self.opt_string();
        let mut c = Component { comment, kind, name, title, width: None, height: None, at: None, lines: Vec::new(), span: Span::default() };
        loop {
            if self.eat_word("width") {
                c.width = Some(self.uint("the width")?);
            } else if self.eat_word("height") {
                c.height = Some(self.uint("the height")?);
            } else if self.eat_word("at") {
                let x = self.uint("x")?;
                if !self.eat(K::COMMA) {
                    let s = self.here();
                    self.err("`at x, y size w x h`", s);
                    return None;
                }
                let y = self.uint("y")?;
                if !self.eat_word("size") {
                    let s = self.here();
                    self.err("`at x, y size w x h`", s);
                    return None;
                }
                let w = self.uint("w")?;
                if !self.eat_word("x") {
                    let s = self.here();
                    self.err("`at x, y size w x h`", s);
                    return None;
                }
                let h = self.uint("h")?;
                c.at = Some([x, y, w, h]);
            } else {
                break;
            }
        }
        let header_end = self.toks[self.pos - 1].end;
        let kind = c.kind.clone();
        let mut lines = Vec::new();
        self.opt_block(|p| {
            if let Some(l) = p.component_line(&kind) {
                lines.push(l);
            }
        });
        c.lines = lines;
        c.span = Span::new(first.start, self.toks.get(self.pos.saturating_sub(1)).map_or(header_end, |t| t.end.max(header_end)));
        Some(c)
    }

    /// A line of a component's block (`kind`: its JSON type), or of a saved view's (`kind`:
    /// `""`).
    fn component_line(&mut self, kind: &str) -> Option<Line> {
        let t = self.peek().unwrap();
        self.captured.clear();
        let saved = kind.is_empty();
        let viewish = saved || kind == "view";
        if t.kind == K::STRING {
            if kind != "markdown" {
                self.fail_line("a text alone is a Markdown box's text");
                return None;
            }
            let s = self.string_of(t);
            self.pos += 1;
            self.end_line();
            return Some(Line::Text(s, Span::new(t.start, t.end)));
        }
        if self.at_raw() {
            let r = self.raw_line();
            self.end_line();
            return r.map(Line::Raw);
        }
        if !saved && self.at_word("apply") {
            self.pos += 1;
            let r = self.apply(t.start);
            self.end_line();
            return r;
        }
        if viewish && self.at_word("compute") && self.nth(1).is_some_and(|n| self.is_word(n)) && self.nth(2).is_some_and(|n| n.kind == K::COLON) {
            self.pos += 1;
            let name = self.word("a name")?;
            self.pos += 1;
            let expr = self.expr();
            self.end_line();
            let expr = expr?;
            let span = Span::new(t.start, expr.span.end);
            return Some(Line::Compute { name, expr, span });
        }
        if viewish && self.at_word("color") && self.nth(1).is_some_and(|n| n.kind == K::STRING) {
            self.pos += 1;
            let color = self.string("a color")?;
            if !self.colon() {
                self.skip_line();
                return None;
            }
            let expr = self.expr();
            self.end_line();
            let expr = expr?;
            let span = Span::new(t.start, expr.span.end);
            return Some(Line::Color { color, expr, span });
        }
        if kind == "form" {
            if self.at_word("default") && self.nth(1).is_some_and(|n| self.is_word(n)) && self.nth(2).is_some_and(|n| n.kind == K::COLON) {
                self.pos += 1;
                let field = self.word("a field")?;
                self.pos += 1;
                let source = self.source();
                let end = self.toks[self.pos - 1].end;
                self.end_line();
                return Some(Line::Default { field, source: source?, span: Span::new(t.start, end) });
            }
            if self.at_word("section") && self.nth(1).is_some_and(|n| n.kind == K::COLON || n.kind == K::STRING) {
                self.pos += 1;
                let title = self.opt_string();
                if !self.colon() {
                    self.skip_line();
                    return None;
                }
                let fields = if self.end_of_line() { Vec::new() } else { self.names()? };
                let end = self.toks[self.pos - 1].end;
                self.end_line();
                let names = std::mem::take(&mut self.captured);
                return Some(Line::Section { title, fields, span: Span::new(t.start, end), names });
            }
            if self.at_word("field") && self.nth(1).is_some_and(|n| self.is_word(n)) {
                return self.form_field();
            }
            if self.at_word("submit") && self.nth(1).is_some_and(|n| n.kind == K::COLON || n.kind == K::STRING) {
                return self.submit();
            }
        }
        if kind == "sheet" && self.at_word("tab") {
            return self.tab().map(Line::Tab);
        }
        // key: value
        let key = self.word("an option")?;
        let opt = if saved { options::find_saved(&key.text).map(|o| (o, "")) } else { options::find(kind, &key.text) };
        let Some((opt, _)) = opt else {
            let known: Vec<&str> = if saved {
                options::SAVED_VIEW.iter().chain(options::VIEW_BODY).map(|o| o.key).collect()
            } else {
                let mut k: Vec<&str> = options::table(kind).iter().map(|o| o.key).collect();
                if kind == "view" {
                    k.extend(options::VIEW_BODY.iter().map(|o| o.key));
                }
                k
            };
            let msg = if known.is_empty() {
                format!("unknown option `{}` (this component's settings are written `config <key>: <value>`)", key.text)
            } else {
                format!("unknown option `{}`; the options here are {}", key.text, known.join(", "))
            };
            self.err(msg, key.span);
            self.skip_line();
            return None;
        };
        if !self.colon() {
            self.skip_line();
            return None;
        }
        let v = self.value(opt.vt, opt.key);
        let end = self.toks[self.pos - 1].end;
        self.end_line();
        let mut names = std::mem::take(&mut self.captured);
        // the key itself is not a name of the value
        if names.first().is_some_and(|n| n.span == key.span) {
            names.remove(0);
        }
        Some(Line::Opt { key, value: v?, span: Span::new(t.start, end), names })
    }

    fn apply(&mut self, start: u32) -> Option<Line> {
        let entity = self.word("an entity (or `auto`)")?;
        if !self.colon() {
            return None;
        }
        if entity.text == "auto" {
            let v = match self.peek().map(|t| self.text(t)) {
                Some("true") => true,
                Some("false") => false,
                _ => {
                    let s = self.here();
                    self.err("`apply auto`: true or false", s);
                    return None;
                }
            };
            self.pos += 1;
            return Some(Line::ApplyAuto(v, Span::new(start, self.toks[self.pos - 1].end)));
        }
        let mut binds = Vec::new();
        loop {
            let name = self.word("a filter")?;
            if !self.eat(K::ARROW) {
                let s = self.here();
                let w = self.what();
                self.err(format!("expected `->`, found {w}"), s);
                return None;
            }
            let path = if self.at(K::NONE_KW) {
                self.pos += 1;
                None
            } else {
                Some(self.path("a field path (or `none`)")?)
            };
            binds.push((name, path));
            if !self.eat(K::COMMA) {
                break;
            }
        }
        Some(Line::Apply { entity, binds, span: Span::new(start, self.toks[self.pos - 1].end) })
    }

    fn form_field(&mut self) -> Option<Line> {
        let t = self.bump().unwrap();
        let comment = self.comments_before(t.start);
        let name = self.word("the field's name")?;
        let label = self.opt_string();
        if !self.colon() {
            self.skip_line();
            return None;
        }
        // the type: up to `where`, `default` or the end of the line
        let ts = self.pos;
        let mut depth = 0i32;
        while !self.end_of_line() {
            let k = self.peek().unwrap();
            if depth == 0 && self.is_word(k) && matches!(self.text(k), "where" | "default") {
                break;
            }
            match k.kind {
                K::LT | K::L_PAREN | K::L_BRACK => depth += 1,
                K::GT | K::R_PAREN | K::R_BRACK => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        if self.pos == ts {
            let s = self.here();
            self.err("expected the field's type", s);
            self.skip_line();
            return None;
        }
        let tspan = Span::new(self.toks[ts].start, self.toks[self.pos - 1].end);
        let ty_text: String = {
            let mut s = String::new();
            for k in ts..self.pos {
                if k > ts && self.toks[k].start > self.toks[k - 1].end {
                    s.push(' ');
                }
                s.push_str(self.text(self.toks[k]));
            }
            s
        };
        let ty = Name { text: ty_text, span: tspan };
        let where_ = if self.at(K::WHERE_KW) {
            self.pos += 1;
            Some(self.expr_until_raw(&["default"])?)
        } else {
            None
        };
        let default = if self.eat_word("default") { Some(self.expr()?) } else { None };
        let end = self.toks[self.pos - 1].end;
        self.end_line();
        Some(Line::Field(FormField { comment, name, label, ty, where_, default, span: Span::new(t.start, end) }))
    }

    /// The text of a `where` condition (`> 0`, `0..10`, `v => …`): kept as written, on one line.
    fn expr_until_raw(&mut self, stop: &[&str]) -> Option<Expr> {
        let start = self.pos;
        let mut depth = 0i32;
        while !self.end_of_line() {
            let t = self.peek().unwrap();
            if depth == 0 && self.is_word(t) && stop.contains(&self.text(t)) {
                break;
            }
            match t.kind {
                K::L_PAREN | K::L_BRACK | K::L_BRACE => depth += 1,
                K::R_PAREN | K::R_BRACK | K::R_BRACE => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        if self.pos == start {
            let s = self.here();
            self.err("expected a condition", s);
            return None;
        }
        let toks = &self.toks[start..self.pos];
        let mut text = String::new();
        let mut map = Vec::new();
        for (k, t) in toks.iter().enumerate() {
            if k > 0 {
                let gap = &self.src[toks[k - 1].end as usize..t.start as usize];
                text.push_str(if gap.contains('\n') { " " } else { gap });
            }
            map.push((text.len() as u32, t.start));
            text.push_str(self.text(*t));
        }
        Some(Expr { text, raw: false, span: Span::new(toks[0].start, toks[toks.len() - 1].end), map })
    }

    /// `submit ["label"]:` and its statements, kept as their text (dedented).
    fn submit(&mut self) -> Option<Line> {
        let t = self.bump().unwrap();
        let label = self.opt_string();
        if !self.colon() {
            self.skip_line();
            return None;
        }
        if !self.end_of_line() {
            self.fail_line("the statements of `submit` go in the block under it");
            return None;
        }
        self.eat(K::NEWLINE);
        if !self.at(K::INDENT) {
            let s = self.here();
            self.err("expected the statements `submit` runs, in an indented block", s);
            return None;
        }
        let first = self.nth(1).map_or(self.src.len() as u32, |x| x.start);
        // the block: up to its DEDENT
        self.pos += 1;
        let mut depth = 1;
        let mut last_end = first;
        while let Some(k) = self.peek() {
            match k.kind {
                K::INDENT => depth += 1,
                K::DEDENT => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                K::NEWLINE => {}
                _ => last_end = k.end,
            }
            self.pos += 1;
        }
        // comments inside the block are the code's
        let line_start = self.src[..first as usize].rfind('\n').map_or(0, |k| k + 1);
        let indent = first as usize - line_start;
        let end_line = self.src[last_end as usize..].find('\n').map_or(self.src.len(), |k| last_end as usize + k);
        for c in self.comments.iter_mut() {
            if c.at as usize >= line_start && (c.at as usize) < end_line {
                c.taken = true;
            }
        }
        let body = &self.src[line_start..end_line];
        let code = body
            .split('\n')
            .map(|l| {
                let l = l.trim_end();
                let strip = l.chars().take(indent).take_while(|c| *c == ' ').count();
                l[strip..].to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.eat(K::DEDENT);
        Some(Line::Submit { label, code, span: Span::new(t.start, end_line as u32), code_span: Span::new(line_start as u32, end_line as u32) })
    }

    // ---- spreadsheets --------------------------------------------------------------------------

    fn tab(&mut self) -> Option<Tab> {
        let t = self.bump().unwrap();
        let comment = self.comments_before(t.start);
        let id = if self.peek().is_some_and(|x| self.is_word(x)) && !matches!(self.peek().map(|x| self.text(x)), Some("fixed" | "free" | "rows" | "cols")) { self.word("a name") } else { None };
        let name = self.opt_string();
        let mut tab = Tab { comment, id, name, mode: None, rows: None, cols: None, lines: Vec::new(), span: Span::default() };
        if self.at_word("fixed") || self.at_word("free") {
            let m = self.bump().unwrap();
            tab.mode = Some(self.text(m).to_string());
        }
        loop {
            if self.at(K::NUMBER) && self.nth_word(1, "x") {
                tab.rows = Some(self.int("rows")? as u64);
                self.pos += 1;
                tab.cols = Some(self.int("columns")? as u64);
            } else if self.eat_word("rows") {
                tab.rows = Some(self.int("rows")? as u64);
            } else if self.eat_word("cols") {
                tab.cols = Some(self.int("columns")? as u64);
            } else {
                break;
            }
        }
        let mut lines = Vec::new();
        self.opt_block(|p| {
            if let Some(l) = p.tab_line() {
                lines.push(l);
            }
        });
        tab.lines = lines;
        tab.span = Span::new(t.start, self.toks.get(self.pos.saturating_sub(1)).map_or(t.end, |x| x.end));
        Some(tab)
    }

    fn tab_line(&mut self) -> Option<TabLine> {
        let t = self.peek().unwrap();
        if self.at_raw() && self.at_word("meta") {
            let r = self.raw_line();
            self.end_line();
            return r.map(TabLine::Raw);
        }
        let w = if self.is_word(t) { self.text(t) } else { "" };
        let is_addr = {
            let letters = w.chars().take_while(|c| c.is_ascii_uppercase()).count();
            (1..=3).contains(&letters) && w.len() > letters && w[letters..].chars().all(|c| c.is_ascii_digit()) && !w[letters..].starts_with('0')
        };
        if is_addr {
            self.pos += 1;
            let mut style = Vec::new();
            while !self.at(K::COLON) && !self.end_of_line() {
                style.push(self.style_item()?);
            }
            if !self.colon() {
                self.skip_line();
                return None;
            }
            let value = if self.at(K::EQ) {
                let eq = self.bump().unwrap();
                let line_end = self.src[eq.end as usize..].find('\n').map_or(self.src.len(), |k| eq.end as usize + k);
                let text = self.src[eq.end as usize..line_end].trim().to_string();
                while self.peek().is_some_and(|x| (x.start as usize) < line_end && !matches!(x.kind, K::NEWLINE | K::INDENT | K::DEDENT)) {
                    self.pos += 1;
                }
                for c in self.comments.iter_mut() {
                    if c.at >= eq.end && (c.at as usize) < line_end {
                        c.taken = true;
                    }
                }
                if !self.end_of_line() {
                    self.fail_line("a cell's formula is one line");
                    return None;
                }
                Some(CellValue::Formula(text, Span::new(eq.end, line_end as u32)))
            } else if self.at(K::STRING) {
                Some(CellValue::Text(self.opt_string().unwrap()))
            } else if self.end_of_line() {
                None
            } else {
                self.fail_line(format!("a cell holds a \"value\" or `= formula`, found {}", self.what()));
                return None;
            };
            let end = self.toks[self.pos - 1].end;
            self.end_line();
            return Some(TabLine::Cell { addr: w.to_string(), style, value, span: Span::new(t.start, end) });
        }
        match w {
            "width" | "height" => {
                self.pos += 1;
                let pairs = self.pairs()?;
                let span = Span::new(t.start, self.toks[self.pos - 1].end);
                self.end_line();
                Some(if w == "width" { TabLine::Widths(pairs, span) } else { TabLine::Heights(pairs, span) })
            }
            "merge" => {
                self.pos += 1;
                let mut out = Vec::new();
                loop {
                    let a = self.word("a cell")?.text;
                    if !self.colon() {
                        return None;
                    }
                    let b = self.word("a cell")?.text;
                    out.push(format!("{a}:{b}"));
                    if !self.eat(K::COMMA) {
                        break;
                    }
                }
                let span = Span::new(t.start, self.toks[self.pos - 1].end);
                self.end_line();
                Some(TabLine::Merge(out, span))
            }
            "freeze" => {
                self.pos += 1;
                let (mut rows, mut cols) = (None, None);
                loop {
                    if self.eat_word("rows") {
                        rows = Some(self.int("rows")?);
                    } else if self.eat_word("cols") {
                        cols = Some(self.int("columns")?);
                    } else {
                        break;
                    }
                }
                let span = Span::new(t.start, self.toks[self.pos - 1].end);
                self.end_line();
                Some(TabLine::Freeze { rows, cols, span })
            }
            _ => {
                self.fail_line(format!("expected a cell (`A1: …`), `width`, `height`, `merge`, `freeze` or `meta`, found {}", self.what()));
                None
            }
        }
    }

    fn style_item(&mut self) -> Option<StyleItem> {
        let s = self.here();
        let w = self.word("a style (bold, italic, color …)")?.text;
        Some(match w.as_str() {
            "bold" | "italic" | "underline" | "strike" | "wrap" => StyleItem::Flag(w),
            "color" | "fill" => StyleItem::Color(w, self.string("a color")?),
            "align" | "valign" => StyleItem::Align(w, self.word("an alignment")?.text),
            "fmt" => {
                let kind = self.word("a number format (general, number, percent, currency, date, text)")?.text;
                let (mut dec, mut thousands, mut date) = (None, false, None);
                loop {
                    if self.eat_word("dec") {
                        dec = Some(self.int("decimals")?);
                    } else if self.eat_word("thousands") {
                        thousands = true;
                    } else if self.at_word("date") && self.nth(1).is_some_and(|t| t.kind == K::STRING) {
                        self.pos += 1;
                        date = self.opt_string();
                    } else {
                        break;
                    }
                }
                StyleItem::Format { kind, dec, thousands, date }
            }
            "border" => StyleItem::Border(self.data()?),
            "style" => StyleItem::Style(self.data()?),
            _ => {
                self.err(format!("`{w}`: a style is bold, italic, underline, strike, wrap, color, fill, align, valign, fmt, border or style"), s);
                return None;
            }
        })
    }

    // ---- saved views ---------------------------------------------------------------------------

    fn saved_view(&mut self, comment: Option<String>) -> Option<SavedView> {
        let start = self.bump().unwrap().start;
        let Some(name) = self.string("the view's name") else {
            self.skip_line();
            return None;
        };
        if !self.eat_word("of") {
            self.fail_line(format!("expected `of <Entity>`, found {}", self.what()));
            return None;
        }
        let Some(entity) = self.word("an entity") else {
            self.skip_line();
            return None;
        };
        let mut lines = Vec::new();
        self.opt_block(|p| {
            if let Some(l) = p.component_line("") {
                lines.push(l);
            }
        });
        let end = self.toks.get(self.pos.saturating_sub(1)).map_or(start, |t| t.end);
        Some(SavedView { comment, name, entity, lines, span: Span::new(start, end) })
    }
}
