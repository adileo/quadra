//! Lexer and lossless parser for the Quadra language.
//!
//! ```
//! let parse = quadra_syntax::parse("entity Customer:\n    name: Text\n");
//! assert!(parse.errors.is_empty());
//! assert_eq!(parse.syntax().to_string(), "entity Customer:\n    name: Text\n");
//! ```

pub mod ast;
pub mod lexer;
mod parser;
mod syntax_kind;

pub use rowan::{TextRange, TextSize};
pub use syntax_kind::{QuadraLanguage, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

impl SyntaxError {
    pub fn new(message: String, range: TextRange) -> SyntaxError {
        SyntaxError { message, range }
    }
}

#[derive(Debug, Clone)]
pub struct Parse {
    pub green: rowan::GreenNode,
    pub errors: Vec<SyntaxError>,
}

impl Parse {
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// Indented tree dump, used by `quadra parse` and snapshot tests.
    pub fn debug_tree(&self) -> String {
        let mut out = String::new();
        for event in self.syntax().preorder_with_tokens() {
            if let rowan::WalkEvent::Enter(el) = event {
                let depth = el.ancestors().count() - 1;
                let indent = "  ".repeat(depth);
                match el {
                    rowan::NodeOrToken::Node(n) => {
                        out.push_str(&format!("{indent}{:?}@{:?}\n", n.kind(), n.text_range()))
                    }
                    rowan::NodeOrToken::Token(t) => {
                        if !t.kind().is_trivia() {
                            out.push_str(&format!("{indent}{:?}@{:?} {:?}\n", t.kind(), t.text_range(), t.text()))
                        }
                    }
                }
            }
        }
        out
    }
}

pub fn parse(text: &str) -> Parse {
    let (tokens, mut errors) = lexer::tokenize(text);
    let (green, parse_errors) = parser::parse(text, &tokens);
    errors.extend(parse_errors);
    errors.sort_by_key(|e| e.range.start());
    Parse { green, errors }
}

/// Parse a single expression (used for f-string interpolations). Returns the expression
/// node, if the text is a valid expression.
pub fn parse_expr(text: &str) -> Option<SyntaxNode> {
    let parse = parse(&format!("const __expr = {text}\n"));
    if !parse.errors.is_empty() {
        return None;
    }
    let root = parse.syntax();
    let value = root.descendants().find(|n| n.kind() == SyntaxKind::VALUE)?;
    value.children().next()
}
