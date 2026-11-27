//! Event-based parser (in the style of rust-analyzer).
//!
//! The grammar functions in `grammar/` see only significant tokens. They emit events;
//! `build_tree` then re-inserts trivia (whitespace, comments) to produce a lossless tree.

mod grammar;

use crate::lexer::Token;
use crate::syntax_kind::SyntaxKind::{self, *};
use crate::SyntaxError;
use rowan::{GreenNode, GreenNodeBuilder, TextRange};

pub(crate) fn parse(src: &str, tokens: &[Token]) -> (GreenNode, Vec<SyntaxError>) {
    let sig: Vec<(SyntaxKind, TextRange)> = tokens
        .iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.range))
        .collect();
    let mut p = Parser::new(src, sig);
    grammar::source_file(&mut p);
    let (events, errors) = p.finish();
    build_tree(src, tokens, events, errors)
}

#[derive(Debug)]
enum Event {
    Start {
        kind: SyntaxKind,
        forward_parent: Option<u32>,
    },
    Finish,
    Token {
        kind: SyntaxKind,
    },
    Error,
    Tombstone,
}

pub(crate) struct Parser<'s> {
    src: &'s str,
    tokens: Vec<(SyntaxKind, TextRange)>,
    pos: usize,
    events: Vec<Event>,
    errors: Vec<(usize, String)>,
    fuel: std::cell::Cell<u32>,
    last_bumped: SyntaxKind,
    /// Inside a `when:` arm condition, `name =>` ends the condition instead of starting a lambda.
    pub(crate) no_bare_lambda: bool,
    /// For each node being built, the height of the tallest child completed so far: what a
    /// node completed now measures (`CompletedMarker::height`).
    heights: Vec<u32>,
    /// Expressions, types, blocks and patterns being parsed inside one another.
    nest: u32,
    /// The current line was already reported as too deeply nested.
    deep: bool,
}

/// How many expressions, types, blocks and patterns may be parsed inside one another. Every
/// later pass (names, types, lowering, the interpreter) walks the tree recursively, so its depth
/// must stay well within a thread's stack: past this the line is an error instead.
pub(crate) const MAX_NEST: u32 = 64;
/// How tall a tree may grow by chaining (`a + b + c …`, `x.f.g …`, `T???`), which builds nested
/// nodes without recursion in the parser.
pub(crate) const MAX_HEIGHT: u32 = 256;

impl<'s> Parser<'s> {
    fn new(src: &'s str, tokens: Vec<(SyntaxKind, TextRange)>) -> Parser<'s> {
        Parser {
            src,
            tokens,
            pos: 0,
            events: Vec::new(),
            errors: Vec::new(),
            fuel: std::cell::Cell::new(256),
            last_bumped: NEWLINE,
            no_bare_lambda: false,
            heights: Vec::new(),
            nest: 0,
            deep: false,
        }
    }

    // ---- depth ----------------------------------------------------------------------

    /// One level deeper (an expression, a type, a block, a pattern inside another): false, with
    /// the rest of the line skipped as an error, when that is too deep.
    pub(crate) fn enter(&mut self) -> bool {
        if self.nest >= MAX_NEST {
            self.too_deep();
            return false;
        }
        self.nest += 1;
        true
    }

    pub(crate) fn leave(&mut self) {
        self.nest -= 1;
    }

    /// Nested too deeply here: one error for the line, and what is nested here skipped as a
    /// whole (up to the bracket, comma or line end that belongs to the enclosing construct),
    /// so that the constructs around it end normally.
    pub(crate) fn too_deep(&mut self) {
        if !self.deep {
            self.error("nested too deeply: split it into smaller expressions or functions");
            self.deep = true;
        }
        let m = self.start();
        let mut open = 0usize;
        loop {
            match self.current() {
                NEWLINE | INDENT | DEDENT => break,
                _ if self.at_eof() => break,
                L_PAREN | L_BRACK | L_BRACE => open += 1,
                R_PAREN | R_BRACK | R_BRACE if open == 0 => break,
                R_PAREN | R_BRACK | R_BRACE => open -= 1,
                COMMA if open == 0 => break,
                _ => {}
            }
            self.bump();
        }
        m.complete(self, ERROR);
    }

    fn finish(self) -> (Vec<Event>, Vec<(usize, String)>) {
        (self.events, self.errors)
    }

    // ---- lookahead ------------------------------------------------------------------

    pub(crate) fn nth(&self, n: usize) -> SyntaxKind {
        let f = self.fuel.get();
        assert!(f > 0, "parser is stuck");
        self.fuel.set(f - 1);
        self.tokens
            .get(self.pos + n)
            .map_or(SyntaxKind::__LAST, |t| t.0)
    }

    pub(crate) fn current(&self) -> SyntaxKind {
        self.nth(0)
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.nth(0) == kind
    }

    pub(crate) fn nth_text(&self, n: usize) -> &'s str {
        self.tokens.get(self.pos + n).map_or("", |t| &self.src[t.1])
    }

    pub(crate) fn nth_range(&self, n: usize) -> Option<TextRange> {
        self.tokens.get(self.pos + n).map(|t| t.1)
    }

    /// Current token is the identifier `text` (a contextual keyword candidate).
    pub(crate) fn at_ctx(&self, text: &str) -> bool {
        self.nth_at_ctx(0, text)
    }

    pub(crate) fn nth_at_ctx(&self, n: usize, text: &str) -> bool {
        self.nth(n) == IDENT && self.nth_text(n) == text
    }

    pub(crate) fn prev_was(&self, kind: SyntaxKind) -> bool {
        self.last_bumped == kind
    }

    // ---- consuming ------------------------------------------------------------------

    pub(crate) fn bump(&mut self) {
        let kind = self.current();
        self.bump_as(kind);
    }

    pub(crate) fn bump_as(&mut self, kind: SyntaxKind) {
        if self.at_eof() {
            return;
        }
        self.fuel.set(256);
        self.last_bumped = self.tokens[self.pos].0;
        if self.last_bumped == NEWLINE {
            self.deep = false;
        }
        self.pos += 1;
        self.events.push(Event::Token { kind });
    }

    /// Bump an identifier as a contextual keyword.
    pub(crate) fn bump_ctx(&mut self) {
        self.bump_as(CONTEXT_KW);
    }

    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.error(format!("expected {}", kind.describe()));
        false
    }

    pub(crate) fn error(&mut self, msg: impl Into<String>) {
        self.errors.push((self.pos, msg.into()));
        self.events.push(Event::Error);
    }

    /// Report an error and wrap the current token in an ERROR node.
    pub(crate) fn err_and_bump(&mut self, msg: impl Into<String>) {
        let m = self.start();
        self.error(msg);
        self.bump();
        m.complete(self, ERROR);
    }

    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len() as u32;
        self.events.push(Event::Tombstone);
        self.heights.push(0);
        Marker {
            pos,
            completed: false,
        }
    }

    /// A node of `height` completed: the node being built around it is at least one taller.
    fn completed(&mut self, height: u32) {
        if let Some(top) = self.heights.last_mut() {
            *top = (*top).max(height);
        }
    }
}

pub(crate) struct Marker {
    pos: u32,
    completed: bool,
}

impl Marker {
    pub(crate) fn complete(mut self, p: &mut Parser, kind: SyntaxKind) -> CompletedMarker {
        self.completed = true;
        p.events[self.pos as usize] = Event::Start {
            kind,
            forward_parent: None,
        };
        p.events.push(Event::Finish);
        let height = p.heights.pop().unwrap_or(0) + 1;
        p.completed(height);
        CompletedMarker {
            pos: self.pos,
            height,
        }
    }

    pub(crate) fn abandon(mut self, p: &mut Parser) {
        self.completed = true;
        if self.pos as usize == p.events.len() - 1 {
            p.events.pop();
        }
        // what it held belongs to the node around it
        let held = p.heights.pop().unwrap_or(0);
        p.completed(held);
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            panic!("marker must be completed or abandoned");
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CompletedMarker {
    pos: u32,
    /// Nodes from this one down to its deepest leaf.
    height: u32,
}

impl CompletedMarker {
    /// Start a new node that will contain this one (for left-recursive constructs).
    pub(crate) fn precede(self, p: &mut Parser) -> Marker {
        let m = p.start();
        if let Event::Start { forward_parent, .. } = &mut p.events[self.pos as usize] {
            *forward_parent = Some(m.pos - self.pos);
        }
        if let Some(top) = p.heights.last_mut() {
            *top = self.height;
        }
        m
    }

    /// Too tall to wrap in another node: chaining more onto it is an error.
    pub(crate) fn too_tall(self) -> bool {
        self.height >= MAX_HEIGHT
    }
}

// ---------------------------------------------------------------------------------------
// Tree building
// ---------------------------------------------------------------------------------------

fn build_tree(
    src: &str,
    tokens: &[Token],
    mut events: Vec<Event>,
    errors: Vec<(usize, String)>,
) -> (GreenNode, Vec<SyntaxError>) {
    let mut builder = GreenNodeBuilder::new();
    let mut ti = 0usize; // index in `tokens` (including trivia)
    let mut sig_seen = 0usize; // significant tokens consumed

    let eat_trivia = |builder: &mut GreenNodeBuilder, ti: &mut usize| {
        while *ti < tokens.len() && tokens[*ti].kind.is_trivia() {
            let t = tokens[*ti];
            builder.token(t.kind.into(), t.text(src));
            *ti += 1;
        }
    };

    // Map from significant token index to its range, for error positions.
    let sig_ranges: Vec<TextRange> = tokens
        .iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| t.range)
        .collect();

    let mut depth = 0usize;
    let mut forward_parents = Vec::new();
    for i in 0..events.len() {
        match std::mem::replace(&mut events[i], Event::Tombstone) {
            Event::Start {
                kind,
                forward_parent,
            } => {
                forward_parents.clear();
                forward_parents.push(kind);
                let mut idx = i;
                let mut fp = forward_parent;
                while let Some(off) = fp {
                    idx += off as usize;
                    fp = match std::mem::replace(&mut events[idx], Event::Tombstone) {
                        Event::Start {
                            kind,
                            forward_parent,
                        } => {
                            forward_parents.push(kind);
                            forward_parent
                        }
                        _ => unreachable!(),
                    };
                }
                if depth > 0 {
                    eat_trivia(&mut builder, &mut ti);
                }
                for kind in forward_parents.drain(..).rev() {
                    builder.start_node(kind.into());
                    depth += 1;
                }
            }
            Event::Finish => {
                depth -= 1;
                if depth == 0 {
                    eat_trivia(&mut builder, &mut ti);
                }
                builder.finish_node();
            }
            Event::Token { kind } => {
                eat_trivia(&mut builder, &mut ti);
                let t = tokens[ti];
                builder.token(kind.into(), t.text(src));
                ti += 1;
                sig_seen += 1;
            }
            Event::Error | Event::Tombstone => {}
        }
    }
    let _ = sig_seen;

    let errors = errors
        .into_iter()
        .map(|(pos, msg)| {
            let range = sig_ranges
                .get(pos)
                .copied()
                .or_else(|| sig_ranges.last().map(|r| TextRange::empty(r.end())))
                .unwrap_or_default();
            SyntaxError::new(msg, range)
        })
        .collect();
    (builder.finish(), errors)
}
