//! Small helpers to navigate the untyped syntax tree.
//!
//! Phase 2 needs only declaration-level structure, so instead of a full typed AST we
//! expose a handful of functions over `SyntaxNode`.

use crate::{SyntaxKind, SyntaxKind::*, SyntaxNode, SyntaxToken};

pub fn child(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
    node.children().find(|n| n.kind() == kind)
}

pub fn children(node: &SyntaxNode, kind: SyntaxKind) -> impl Iterator<Item = SyntaxNode> {
    node.children().filter(move |n| n.kind() == kind)
}

pub fn token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == kind)
}

/// First keyword token of a node (reserved or contextual).
pub fn keyword(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind().is_keyword())
}

/// Text of the first contextual keyword (`check`, `post`, `key`…).
pub fn ctx_keyword(node: &SyntaxNode) -> Option<String> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == CONTEXT_KW)
        .map(|t| t.text().to_string())
}

/// The identifier token of a NAME / NAME_REF child.
pub fn name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    child(node, NAME).and_then(|n| token(&n, IDENT))
}

pub fn name_text(node: &SyntaxNode) -> Option<String> {
    name_token(node).map(|t| t.text().to_string())
}

/// Identifier tokens of a PATH node, in order.
pub fn path_segments(path: &SyntaxNode) -> Vec<SyntaxToken> {
    children(path, NAME_REF)
        .filter_map(|n| token(&n, IDENT))
        .collect()
}

/// The event a `publish Product to shop` declares: `PublishProductToShop`, with the record's
/// name and the connection's in capitals (`erp_eu` → `ErpEu`).
pub fn publish_event(item: &SyntaxNode) -> Option<(String, rowan::TextRange)> {
    let entity = child(item, SyntaxKind::PATH)?;
    let conn = children(item, SyntaxKind::NAME).next()?;
    let cap: String = conn
        .text()
        .to_string()
        .trim()
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    Some((
        format!("Publish{}To{cap}", path_text(&entity)),
        entity.text_range(),
    ))
}

pub fn path_text(path: &SyntaxNode) -> String {
    path_segments(path)
        .iter()
        .map(|t| t.text().to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// Comment lines immediately preceding a node (its documentation).
pub fn doc_comment(node: &SyntaxNode) -> Option<String> {
    let mut lines = Vec::new();
    let mut tok = node.first_token()?.prev_token();
    let mut newlines = 0;
    while let Some(t) = tok {
        match t.kind() {
            COMMENT => {
                if !comment_on_own_line(&t) {
                    break;
                }
                lines.push(t.text().trim_start_matches('#').trim().to_string());
                newlines = 0;
            }
            WHITESPACE | NEWLINE => {
                newlines += t.text().matches('\n').count();
                if newlines > 1 {
                    break;
                }
            }
            INDENT | DEDENT => {}
            _ => break,
        }
        tok = t.prev_token();
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

fn comment_on_own_line(comment: &SyntaxToken) -> bool {
    let mut prev = comment.prev_token();
    while let Some(t) = prev {
        match t.kind() {
            WHITESPACE if !t.text().contains('\n') => prev = t.prev_token(),
            WHITESPACE | NEWLINE | INDENT | DEDENT => return true,
            _ => return false,
        }
    }
    true
}

/// The first line of a declaration, without comments (for hovers and outlines).
/// Line breaks inside brackets are folded into spaces.
pub fn header(node: &SyntaxNode) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for tok in node
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        match tok.kind() {
            COMMENT | INDENT | DEDENT => continue,
            NEWLINE => break,
            L_PAREN | L_BRACK | L_BRACE => depth += 1,
            R_PAREN | R_BRACK | R_BRACE => depth -= 1,
            WHITESPACE if tok.text().contains('\n') => {
                if depth > 0 {
                    out.push(' ');
                    continue;
                }
                break;
            }
            _ => {}
        }
        out.push_str(tok.text());
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_end_matches(':').trim_end().to_string()
}
