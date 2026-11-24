//! Semantic tokens: precise colouring on top of the TextMate grammar. They tell apart what
//! a regex cannot: entities, variants, stored fields, formulas, actions and units.

use crate::line_index::LineIndex;
use quadra_sema::{Analysis, DeclKind, FileId, MemberKind, Project, Target, BUILTIN_TYPE_NAMES};
use quadra_syntax::{SyntaxKind::*, SyntaxNode, SyntaxToken};
use std::collections::HashMap;
use tower_lsp::lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend};

const TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,        // 0
    SemanticTokenType::TYPE,           // 1
    SemanticTokenType::ENUM_MEMBER,    // 2
    SemanticTokenType::PROPERTY,       // 3
    SemanticTokenType::FUNCTION,       // 4
    SemanticTokenType::METHOD,         // 5
    SemanticTokenType::VARIABLE,       // 6
    SemanticTokenType::STRING,         // 7
    SemanticTokenType::NUMBER,         // 8
    SemanticTokenType::COMMENT,        // 9
    SemanticTokenType::TYPE_PARAMETER, // 10: units and currencies
    SemanticTokenType::NAMESPACE,      // 11
    SemanticTokenType::INTERFACE,      // 12
    SemanticTokenType::ENUM,           // 13
    SemanticTokenType::CLASS,          // 14
];

const MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,     // bit 0
    SemanticTokenModifier::READONLY,        // bit 1: formulas
    SemanticTokenModifier::DEFAULT_LIBRARY, // bit 2: built-ins
];

const T_KEYWORD: u32 = 0;
const T_TYPE: u32 = 1;
const T_VARIANT: u32 = 2;
const T_PROPERTY: u32 = 3;
const T_FUNCTION: u32 = 4;
const T_METHOD: u32 = 5;
const T_VARIABLE: u32 = 6;
const T_STRING: u32 = 7;
const T_NUMBER: u32 = 8;
const T_COMMENT: u32 = 9;
const T_UNIT: u32 = 10;
const T_NAMESPACE: u32 = 11;
const T_INTERFACE: u32 = 12;
const T_ENUM: u32 = 13;
const T_CLASS: u32 = 14;

const T_DECL: u32 = 1;
const T_READONLY: u32 = 2;
const T_BUILTIN: u32 = 4;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend { token_types: TYPES.to_vec(), token_modifiers: MODIFIERS.to_vec() }
}

fn decl_type(kind: DeclKind) -> u32 {
    match kind {
        DeclKind::Entity | DeclKind::Record => T_CLASS,
        DeclKind::Trait => T_INTERFACE,
        DeclKind::Enum => T_ENUM,
        DeclKind::Ledger => T_TYPE,
        DeclKind::Fn => T_FUNCTION,
        DeclKind::Const => T_VARIABLE,
        _ => T_NAMESPACE,
    }
}

fn member_type(kind: MemberKind) -> (u32, u32) {
    match kind {
        MemberKind::Formula => (T_PROPERTY, T_READONLY),
        MemberKind::Action | MemberKind::FlowAction | MemberKind::Method => (T_METHOD, 0),
        _ => (T_PROPERTY, 0),
    }
}

pub fn tokens(project: &Project, a: &Analysis, file: FileId) -> Vec<SemanticToken> {
    let src = project.file(file);
    let index = LineIndex::new(&src.text);
    let refs: HashMap<_, _> =
        a.references.iter().filter(|r| r.file == file).map(|r| (r.range, r.target.clone())).collect();
    let decl_names: HashMap<_, _> =
        a.decls.iter().filter(|d| d.file == file).map(|d| (d.name_range, d.kind)).collect();
    let member_names: HashMap<_, _> = a
        .members
        .values()
        .flatten()
        .filter(|m| m.file == file && m.kind != MemberKind::FlowAction)
        .map(|m| (m.name_range, m.kind))
        .collect();

    let mut out = Vec::new();
    let (mut prev_line, mut prev_col) = (0u32, 0u32);
    let root: SyntaxNode = src.parse.syntax();
    for tok in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        let Some((ty, mods)) = classify(&tok, a, &refs, &decl_names, &member_names) else { continue };
        let text = tok.text();
        if text.is_empty() || text.contains('\n') {
            continue; // multi-line tokens are left to the TextMate grammar
        }
        let (line, col) = index.line_col(tok.text_range().start());
        let length: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { col - prev_col } else { col };
        out.push(SemanticToken { delta_line, delta_start, length, token_type: ty, token_modifiers_bitset: mods });
        prev_line = line;
        prev_col = col;
    }
    out
}

fn classify(
    tok: &SyntaxToken,
    a: &Analysis,
    refs: &HashMap<quadra_syntax::TextRange, Target>,
    decl_names: &HashMap<quadra_syntax::TextRange, DeclKind>,
    member_names: &HashMap<quadra_syntax::TextRange, MemberKind>,
) -> Option<(u32, u32)> {
    let kind = tok.kind();
    if kind.is_keyword() {
        return Some((T_KEYWORD, 0));
    }
    match kind {
        COMMENT => return Some((T_COMMENT, 0)),
        STRING | FSTRING => return Some((T_STRING, 0)),
        NUMBER | PERCENT | DATE | DATETIME | YEAR_MONTH => return Some((T_NUMBER, 0)),
        IDENT => {}
        _ => return None,
    }
    let range = tok.text_range();
    if let Some(target) = refs.get(&range) {
        return Some(match target {
            Target::Decl(d) => (decl_type(a.decl(*d).kind), 0),
            Target::Member(o, i) => member_type(a.member(*o, *i).kind),
            Target::Variant(_) => (T_VARIANT, 0),
        });
    }
    if let Some(k) = decl_names.get(&range) {
        return Some((decl_type(*k), T_DECL));
    }
    if let Some(k) = member_names.get(&range) {
        let (t, m) = member_type(*k);
        return Some((t, m | T_DECL));
    }
    let parent = tok.parent()?;
    let grand = parent.parent();
    let text = tok.text();
    Some(match parent.kind() {
        UNIT => (T_UNIT, 0),
        NAME_REF if grand.as_ref().map(|g| g.kind()) == Some(PATH)
            && grand.as_ref().and_then(|g| g.parent()).map(|p| p.kind()) == Some(IMPORT) =>
        {
            (T_NAMESPACE, 0)
        }
        NAME_REF if grand.as_ref().map(|g| g.kind()) == Some(PATH)
            && grand.as_ref().and_then(|g| g.parent()).map(|p| p.kind()) == Some(TYPE_PATH) =>
        {
            if BUILTIN_TYPE_NAMES.contains(&text) {
                (T_TYPE, T_BUILTIN)
            } else if text.chars().next().map_or(false, char::is_lowercase) {
                (T_PROPERTY, 0) // `Money<order.currency>`
            } else {
                (T_UNIT, 0) // `Money<EUR>`, `Qty<pcs>`
            }
        }
        NAME_REF => {
            let is_member = parent
                .prev_sibling_or_token()
                .map_or(false, |p| matches!(p.kind(), DOT | QUESTION_DOT));
            let called = grand.as_ref().map_or(false, |g| g.kind() == CALL_EXPR)
                && (is_member && grand.as_ref().and_then(|g| g.parent()).map_or(false, |p| p.kind() == CALL_EXPR)
                    || !is_member);
            let callee = if is_member {
                grand.as_ref().and_then(|g| g.parent()).map_or(false, |p| p.kind() == CALL_EXPR)
            } else {
                called
            };
            if BUILTIN_TYPE_NAMES.contains(&text) {
                (T_TYPE, T_BUILTIN)
            } else if callee {
                (if is_member { T_METHOD } else { T_FUNCTION }, 0)
            } else if is_member {
                (T_PROPERTY, 0)
            } else {
                (T_VARIABLE, 0)
            }
        }
        NAME if grand.as_ref().map(|g| g.kind()) == Some(ARG) => (T_PROPERTY, 0), // named argument
        NAME if grand.as_ref().map(|g| g.kind()) == Some(PARAM) => (T_VARIABLE, T_DECL),
        NAME => (T_VARIABLE, T_DECL),
        _ => return None,
    })
}
