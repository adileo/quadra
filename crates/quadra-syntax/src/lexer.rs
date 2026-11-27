//! Lexer: raw tokenization followed by a layout pass that decides which line breaks are
//! significant and inserts `INDENT` / `DEDENT` tokens (see the syntax reference (quadra.run/reference/syntax) §1).
//!
//! The output covers the whole input: concatenating the token texts gives back the source.
//! `INDENT`, `DEDENT` and a final synthetic `NEWLINE` are zero-width.

use crate::syntax_kind::SyntaxKind::{self, *};
use crate::SyntaxError;
use rowan::{TextRange, TextSize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

impl Token {
    fn new(kind: SyntaxKind, start: usize, end: usize) -> Token {
        Token {
            kind,
            range: TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32)),
        }
    }
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.range]
    }
}

pub fn tokenize(src: &str) -> (Vec<Token>, Vec<SyntaxError>) {
    let mut errors = Vec::new();
    let raw = raw_tokens(src, &mut errors);
    let tokens = layout(src, raw, &mut errors);
    (tokens, errors)
}

// ---------------------------------------------------------------------------------------
// Raw tokens
// ---------------------------------------------------------------------------------------

/// Raw line breaks are emitted as NEWLINE; the layout pass turns the insignificant ones
/// into WHITESPACE.
fn raw_tokens(src: &str, errors: &mut Vec<SyntaxError>) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let c = src[i..].chars().next().unwrap();
        let kind = match c {
            '\n' => {
                i += 1;
                NEWLINE
            }
            '\r' if bytes.get(i + 1) == Some(&b'\n') => {
                i += 2;
                NEWLINE
            }
            ' ' | '\t' | '\r' => {
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r') {
                    if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                        break;
                    }
                    i += 1;
                }
                WHITESPACE
            }
            '#' => {
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
                COMMENT
            }
            '"' => {
                i = lex_string(src, i, errors);
                STRING
            }
            'f' if bytes.get(i + 1) == Some(&b'"') => {
                i = lex_string(src, i + 1, errors);
                FSTRING
            }
            c if c.is_ascii_digit() => lex_number(bytes, &mut i),
            c if c.is_alphabetic() || c == '_' => {
                while i < bytes.len() {
                    let ch = src[i..].chars().next().unwrap();
                    if ch.is_alphanumeric() || ch == '_' {
                        i += ch.len_utf8();
                    } else {
                        break;
                    }
                }
                SyntaxKind::from_keyword(&src[start..i]).unwrap_or(IDENT)
            }
            _ => {
                let (kind, len) = punct(&bytes[i..]);
                if kind == ERROR_TOKEN {
                    let len = c.len_utf8();
                    errors.push(SyntaxError::new(
                        format!("unexpected character `{c}`"),
                        range(start, start + len),
                    ));
                    i += len;
                } else {
                    i += len;
                }
                kind
            }
        };
        tokens.push(Token::new(kind, start, i));
    }
    tokens
}

fn range(start: usize, end: usize) -> TextRange {
    TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32))
}

/// `i` points at the opening quote. Returns the index after the closing quote.
fn lex_string(src: &str, i: usize, errors: &mut Vec<SyntaxError>) -> usize {
    let bytes = src.as_bytes();
    let triple = src[i..].starts_with("\"\"\"");
    let mut j = if triple { i + 3 } else { i + 1 };
    loop {
        if j >= bytes.len() {
            errors.push(SyntaxError::new("unterminated string".into(), range(i, j)));
            return j;
        }
        match bytes[j] {
            b'\\' => j += 2,
            b'"' if triple => {
                if src[j..].starts_with("\"\"\"") {
                    return j + 3;
                }
                j += 1;
            }
            b'"' => return j + 1,
            b'\n' if !triple => {
                errors.push(SyntaxError::new("unterminated string".into(), range(i, j)));
                return j;
            }
            _ => j += 1,
        }
    }
}

fn digits(bytes: &[u8], i: &mut usize) -> usize {
    let start = *i;
    while *i < bytes.len() && (bytes[*i].is_ascii_digit() || bytes[*i] == b'_') {
        *i += 1;
    }
    *i - start
}

fn exactly_digits(bytes: &[u8], at: usize, n: usize) -> bool {
    at + n <= bytes.len()
        && bytes[at..at + n].iter().all(u8::is_ascii_digit)
        && bytes.get(at + n).map_or(true, |b| !b.is_ascii_digit())
}

fn lex_number(bytes: &[u8], i: &mut usize) -> SyntaxKind {
    let start = *i;
    // Dates: 2026-11-21, date-times: 2026-11-21 09:00, year-month: 2026-11
    if exactly_digits(bytes, start, 4)
        && bytes.get(start + 4) == Some(&b'-')
        && exactly_digits(bytes, start + 5, 2)
    {
        if bytes.get(start + 7) == Some(&b'-') && exactly_digits(bytes, start + 8, 2) {
            *i = start + 10;
            let sep = bytes.get(*i).copied();
            if matches!(sep, Some(b' ') | Some(b'T'))
                && exactly_digits(bytes, *i + 1, 2)
                && bytes.get(*i + 3) == Some(&b':')
                && exactly_digits(bytes, *i + 4, 2)
            {
                *i += 6;
                if bytes.get(*i) == Some(&b':') && exactly_digits(bytes, *i + 1, 2) {
                    *i += 3;
                }
                return DATETIME;
            }
            return DATE;
        }
        *i = start + 7;
        return YEAR_MONTH;
    }
    digits(bytes, i);
    if bytes.get(*i) == Some(&b'.') && bytes.get(*i + 1).map_or(false, u8::is_ascii_digit) {
        *i += 1;
        digits(bytes, i);
    }
    if bytes.get(*i) == Some(&b'%') {
        *i += 1;
        return PERCENT;
    }
    NUMBER
}

fn punct(b: &[u8]) -> (SyntaxKind, usize) {
    let two = if b.len() >= 2 { &b[..2] } else { &b[..1] };
    let k2 = match two {
        b".." => Some(DOT2),
        b"?." => Some(QUESTION_DOT),
        b"??" => Some(QUESTION2),
        b"->" => Some(ARROW),
        b"=>" => Some(FAT_ARROW),
        b":=" => Some(COLON_EQ),
        b"+=" => Some(PLUS_EQ),
        b"-=" => Some(MINUS_EQ),
        b"==" => Some(EQ2),
        b"!=" => Some(NEQ),
        b"<=" => Some(LE),
        b">=" => Some(GE),
        _ => None,
    };
    if let Some(k) = k2 {
        return (k, 2);
    }
    let k = match b[0] {
        b'(' => L_PAREN,
        b')' => R_PAREN,
        b'[' => L_BRACK,
        b']' => R_BRACK,
        b'{' => L_BRACE,
        b'}' => R_BRACE,
        b',' => COMMA,
        b':' => COLON,
        b'.' => DOT,
        b'?' => QUESTION,
        b'=' => EQ,
        b'<' => LT,
        b'>' => GT,
        b'+' => PLUS,
        b'-' => MINUS,
        b'*' => STAR,
        b'/' => SLASH,
        b'|' => PIPE,
        b'@' => AT,
        _ => ERROR_TOKEN,
    };
    (k, 1)
}

// ---------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------

fn is_binary_op(kind: SyntaxKind, text: &str) -> bool {
    matches!(
        kind,
        PLUS | MINUS | STAR | SLASH | QUESTION2 | EQ2 | NEQ | LT | GT | LE | GE | AND_KW | OR_KW
    ) || (kind == IDENT && text == "implies")
}

/// The previous line ends with a token that cannot end an expression.
/// `>` and `*` are excluded: they legitimately end a line (`Money<CC>`, `read *`).
fn continues_after(kind: SyntaxKind, text: &str) -> bool {
    kind == COMMA || (is_binary_op(kind, text) && !matches!(kind, GT | LT | STAR))
}

fn layout(src: &str, raw: Vec<Token>, errors: &mut Vec<SyntaxError>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(raw.len() + 64);
    let mut stack: Vec<usize> = vec![0];
    let mut depth: usize = 0;
    let mut last_sig: Option<usize> = None;
    let mut pending_nl: Option<usize> = None;
    let mut at_line_start = true;
    let mut indent_col = 0usize;
    let mut indent_tab: Option<TextRange> = None;

    let next_sig = |from: usize| -> Option<Token> {
        raw[from..]
            .iter()
            .copied()
            .find(|t| !matches!(t.kind, WHITESPACE | COMMENT | NEWLINE))
    };

    for (ri, tok) in raw.iter().copied().enumerate() {
        match tok.kind {
            NEWLINE => {
                out.push(Token {
                    kind: WHITESPACE,
                    ..tok
                });
                if pending_nl.is_none() && depth == 0 && last_sig.is_some() {
                    pending_nl = Some(out.len() - 1);
                }
                at_line_start = true;
                indent_col = 0;
                indent_tab = None;
            }
            WHITESPACE => {
                if at_line_start {
                    let text = tok.text(src);
                    indent_col = text.len();
                    if text.contains('\t') {
                        indent_tab = Some(tok.range);
                    }
                }
                out.push(tok);
            }
            COMMENT => out.push(tok),
            _ => {
                if at_line_start && depth == 0 {
                    match last_sig {
                        None => {
                            if indent_col > 0 {
                                errors.push(SyntaxError::new(
                                    "unexpected indentation".into(),
                                    tok.range,
                                ));
                            }
                        }
                        Some(ls) => {
                            let prev = out[ls];
                            let text = tok.text(src);
                            let starts_cont = match tok.kind {
                                DOT | QUESTION_DOT => true,
                                // `else:`, `else =>` and `else if` start a branch; any other
                                // `else` continues a ternary from the line above
                                ELSE_KW => !matches!(
                                    next_sig(ri + 1).map(|t| t.kind),
                                    Some(COLON) | Some(FAT_ARROW) | Some(IF_KW)
                                ),
                                k => is_binary_op(k, text),
                            };
                            let continuation =
                                continues_after(prev.kind, prev.text(src)) || starts_cont;
                            if continuation {
                                // the line break stays trivia
                            } else {
                                if let Some(nl) = pending_nl {
                                    out[nl].kind = NEWLINE;
                                }
                                if let Some(tab) = indent_tab {
                                    errors.push(SyntaxError::new(
                                        "tabs are not allowed in indentation, use spaces".into(),
                                        tab,
                                    ));
                                }
                                let at = tok.range.start();
                                let top = *stack.last().unwrap();
                                if indent_col > top {
                                    stack.push(indent_col);
                                    out.push(Token {
                                        kind: INDENT,
                                        range: TextRange::empty(at),
                                    });
                                } else if indent_col < top {
                                    while indent_col < *stack.last().unwrap() {
                                        stack.pop();
                                        out.push(Token {
                                            kind: DEDENT,
                                            range: TextRange::empty(at),
                                        });
                                    }
                                    if indent_col != *stack.last().unwrap() {
                                        errors.push(SyntaxError::new(
                                            "this line's indentation does not match any outer block".into(),
                                            tok.range,
                                        ));
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
                    L_PAREN | L_BRACK | L_BRACE => depth += 1,
                    R_PAREN | R_BRACK | R_BRACE => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
    }

    let end = TextSize::from(src.len() as u32);
    if last_sig.is_some() {
        match pending_nl {
            Some(nl) => out[nl].kind = NEWLINE,
            None => out.push(Token {
                kind: NEWLINE,
                range: TextRange::empty(end),
            }),
        }
        for _ in 1..stack.len() {
            out.push(Token {
                kind: DEDENT,
                range: TextRange::empty(end),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<SyntaxKind> {
        let (toks, errs) = tokenize(src);
        assert!(errs.is_empty(), "{errs:?}");
        toks.into_iter()
            .map(|t| t.kind)
            .filter(|k| !k.is_trivia())
            .collect()
    }

    #[test]
    fn indentation() {
        let k = kinds("entity A:\n    x: Int\n    y: Int\nfn f():\n    return 1\n");
        assert_eq!(
            k,
            vec![
                ENTITY_KW, IDENT, COLON, NEWLINE, INDENT, IDENT, COLON, IDENT, NEWLINE, IDENT,
                COLON, IDENT, NEWLINE, DEDENT, FN_KW, IDENT, L_PAREN, R_PAREN, COLON, NEWLINE,
                INDENT, RETURN_KW, NUMBER, NEWLINE, DEDENT
            ]
        );
    }

    #[test]
    fn continuation_and_literals() {
        let k =
            kinds("x := a\n    .where(b)\n    + 2 kg\ny = 9.90 EUR/pcs\nz = 2026-11-21 09:00\n");
        assert_eq!(
            k,
            vec![
                IDENT, COLON_EQ, IDENT, DOT, WHERE_KW, L_PAREN, IDENT, R_PAREN, PLUS, NUMBER,
                IDENT, NEWLINE, IDENT, EQ, NUMBER, IDENT, SLASH, IDENT, NEWLINE, IDENT, EQ,
                DATETIME, NEWLINE
            ]
        );
    }

    #[test]
    fn else_arm_is_not_a_continuation() {
        let k = kinds("x := when:\n    a => 1\n    else => 2\n");
        assert_eq!(
            k,
            vec![
                IDENT, COLON_EQ, WHEN_KW, COLON, NEWLINE, INDENT, IDENT, FAT_ARROW, NUMBER,
                NEWLINE, ELSE_KW, FAT_ARROW, NUMBER, NEWLINE, DEDENT
            ]
        );
    }

    #[test]
    fn lossless() {
        let src = "entity A:  # c\n\n    x: Int\r\n";
        let (toks, _) = tokenize(src);
        let joined: String = toks.iter().map(|t| t.text(src)).collect();
        assert_eq!(joined, src);
    }
}

#[cfg(test)]
mod at_tests {
    use super::*;

    #[test]
    fn at_name() {
        let (toks, errs) = tokenize("x = @region.count()\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(toks.iter().any(|t| t.kind == AT));
        let e = crate::parse_expr("@region.count() + 1").expect("an expression");
        assert!(e.descendants().any(|n| n.kind() == SyntaxKind::PARAM_REF));
        assert!(crate::parse_expr("@ 1").is_none());
    }
}
