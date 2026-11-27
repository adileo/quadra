//! `.qdi`: Quadra interface files. An app (pages of components on the 24-column grid) or saved
//! views of entities, as text in the project's `interfaces/` directory, with the same lexer,
//! comments, literals and expressions as `.qd` (the interface files reference (quadra.run/reference/interfaces) is the specification).
//!
//! This crate reads and writes them, and converts them to and from the JSON configurations
//! the server keeps (`_quadra_views`, the export of `quadra db views export`):
//!
//! ```
//! let src = "app \"Hello\":\n    page main \"Main\":\n        row height 2:\n            markdown intro width 24:\n                \"Hello\"\n";
//! let defs = quadra_qdi::to_json(src).unwrap();
//! assert_eq!(defs[0]["config"]["pages"][0]["components"][0]["config"]["text"], "Hello");
//! assert_eq!(quadra_qdi::from_json(&defs).unwrap().0, src);
//! ```
//!
//! - `parse` → the syntax tree and the problems found (with their spans);
//! - `print` → the canonical text of a tree (what the UI writes back, what `format` gives);
//! - `lower` → the JSON definitions of a tree (`{kind, entity, type, name, description,
//!   config, shares}` each); `lift` → the tree of JSON definitions.
//!
//! Checking what the expressions mean (fields, types, `@params`) is the type checker's job:
//! this crate only reads them as Quadra expressions.

pub mod ast;
pub mod check;
pub mod layout;
pub mod lift;
pub mod lower;
pub mod options;
pub mod parse;
pub mod print;
pub mod text;

pub use ast::{File, Span};
use serde_json::Value as J;

/// A problem of a file, at a place of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub message: String,
    pub span: Span,
}

impl Diag {
    pub fn new(message: impl Into<String>, span: Span) -> Diag {
        Diag {
            message: message.into(),
            span,
        }
    }
}

impl std::fmt::Display for Diag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (at {}..{})",
            self.message, self.span.start, self.span.end
        )
    }
}

/// The 1-based line and column (in characters) of a byte offset.
pub fn line_col(src: &str, offset: u32) -> (usize, usize) {
    let at = (offset as usize).min(src.len());
    let before = &src[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// Read a file: its tree, and what is wrong with it (empty: nothing).
pub fn parse(src: &str) -> (File, Vec<Diag>) {
    parse::parse(src)
}

/// The canonical text of a tree.
pub fn print(file: &File) -> String {
    print::print(file)
}

/// The JSON definitions a tree stands for.
pub fn lower(file: &File) -> Result<Vec<J>, Vec<Diag>> {
    lower::lower(file)
}

/// The tree of JSON definitions (apps and saved views), and the warnings about what a file
/// does not keep (shares with users and teams, owners).
pub fn lift(defs: &[J]) -> Result<(File, Vec<String>), String> {
    lift::lift(defs)
}

/// A file's JSON definitions.
pub fn to_json(src: &str) -> Result<Vec<J>, Vec<Diag>> {
    let (file, diags) = parse(src);
    if !diags.is_empty() {
        return Err(diags);
    }
    lower(&file)
}

/// JSON definitions as a file (canonical), with the warnings of `lift`.
pub fn from_json(defs: &[J]) -> Result<(String, Vec<String>), String> {
    let (file, warnings) = lift(defs)?;
    Ok((print(&file), warnings))
}

/// A file in its canonical form (the same definitions, written as the UI writes them).
pub fn format(src: &str) -> Result<String, Vec<Diag>> {
    let defs = to_json(src)?;
    from_json(&defs)
        .map(|(t, _)| t)
        .map_err(|m| vec![Diag::new(m, Span::default())])
}
