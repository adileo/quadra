//! Strings, names and data literals as `.qdi` files write them.
//!
//! - A string is `"…"` with the escapes of Quadra strings (`\n \t \r \" \\`, and `\{ \}`
//!   kept as braces) plus `\u{XXXX}` for any other control character. A text with line breaks
//!   (a Markdown box) is written as a text block: `"""` on its own line, the lines of the text
//!   at the indentation of the `"""`, and `"""` again on its own line.
//! - A data literal is JSON written the Quadra way: `none`, `true`, `false`, numbers, strings
//!   (bare when they are an identifier or a date: `Confirmed`, `2026-01-31`), lists `[a, b]`,
//!   objects `{key: value}`.

use serde_json::Value as J;

/// An identifier as Quadra's lexer reads one: a letter or `_`, then letters, digits and `_`.
pub fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_alphabetic() || c == '_') && cs.all(|c| c.is_alphanumeric() || c == '_')
}

/// `customer.region`: identifiers joined by dots.
pub fn is_path(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(is_ident)
}

/// `2026-11-21`, exactly (the lexer reads it as one date token).
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(k, c)| k == 4 || k == 7 || c.is_ascii_digit())
}

/// Words that a data literal reads as something other than a string.
const RESERVED: &[&str] = &["none", "null", "true", "false"];

/// A string as a data literal writes it: bare when it reads back as the same string.
pub fn bare_ok(s: &str) -> bool {
    (is_ident(s) && !RESERVED.contains(&s)) || is_date(s)
}

/// `"…"` with escapes.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The text of a string token's inside, its escapes processed (`\u{…}` included); any other
/// backslash is kept as written, as Quadra does.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(c @ ('"' | '\\' | '{' | '}')) => out.push(c),
            Some('u') if chars.peek() == Some(&'{') => {
                chars.next();
                let mut hex = String::new();
                while let Some(&h) = chars.peek() {
                    chars.next();
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                }
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => {
                        out.push_str("\\u{");
                        out.push_str(&hex);
                        out.push('}');
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Can `s` be written as a text block (it has line breaks, and nothing a block cannot hold)?
pub fn block_ok(s: &str) -> bool {
    s.contains('\n') && !s.chars().any(|c| c == '\r' || ((c as u32) < 0x20 && c != '\n' && c != '\t') || c as u32 == 0x7f)
}

/// A text block at `indent` spaces: the lines of the opening `"""`, the text, the closing `"""`.
pub fn text_block(s: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut esc = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k];
        if c == '\\' {
            esc.push_str("\\\\");
        } else if c == '"' {
            // a run of three quotes would close the block: escape every quote of such a run
            let mut run = 0;
            while k + run < chars.len() && chars[k + run] == '"' {
                run += 1;
            }
            for _ in 0..run {
                esc.push_str(if run >= 3 { "\\\"" } else { "\"" });
            }
            k += run;
            continue;
        } else {
            esc.push(c);
        }
        k += 1;
    }
    let mut out = format!("{pad}\"\"\"\n");
    for line in esc.split('\n') {
        if !line.is_empty() {
            out.push_str(&pad);
            out.push_str(line);
        }
        out.push('\n');
    }
    out.push_str(&pad);
    out.push_str("\"\"\"");
    out
}

/// The text of a string token (`"…"` or a text block starting at column `col`).
pub fn string_value(tok: &str, col: usize) -> String {
    if let Some(rest) = tok.strip_prefix("\"\"\"") {
        let inner = if tok.len() >= 6 && tok.ends_with("\"\"\"") { &tok[3..tok.len() - 3] } else { rest };
        let Some(inner) = inner.strip_prefix('\n') else {
            return unescape(inner);
        };
        // the line break and the indentation before the closing quotes are the block's
        let inner = match inner.rfind('\n') {
            Some(k) if inner[k + 1..].chars().all(|c| c == ' ') => &inner[..k],
            _ => inner,
        };
        let lines: Vec<String> = inner
            .split('\n')
            .map(|l| {
                let strip = l.chars().take(col).take_while(|c| *c == ' ').count();
                l[strip..].to_string()
            })
            .collect();
        return unescape(&lines.join("\n"));
    }
    let inner = if tok.len() >= 2 && tok.ends_with('"') { &tok[1..tok.len() - 1] } else { tok.get(1..).unwrap_or("") };
    unescape(inner)
}

/// A JSON value as a data literal, on one line.
pub fn data(v: &J) -> String {
    match v {
        J::Null => "none".into(),
        J::Bool(b) => b.to_string(),
        J::Number(n) => n.to_string(),
        J::String(s) => {
            if bare_ok(s) {
                s.clone()
            } else {
                quote(s)
            }
        }
        J::Array(a) => format!("[{}]", a.iter().map(data).collect::<Vec<_>>().join(", ")),
        J::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", key(k), data(v))).collect::<Vec<_>>().join(", ")),
    }
}

/// A key of an object, a segment of a path: bare when it is an identifier.
pub fn key(k: &str) -> String {
    if is_ident(k) {
        k.to_string()
    } else {
        quote(k)
    }
}

/// A list of names (fields, filters): `a, b.c` when every one is a path, else a data list.
pub fn names(list: &[String]) -> String {
    if !list.is_empty() && list.iter().all(|s| is_path(s)) {
        list.join(", ")
    } else {
        data(&J::Array(list.iter().map(|s| J::String(s.clone())).collect()))
    }
}

/// A key of a list of pairs (`widths: number 120`): a path or a row number, bare.
pub fn pair_key(k: &str) -> String {
    if is_path(k) || (!k.is_empty() && k.len() < 10 && k.chars().all(|c| c.is_ascii_digit()) && !k.starts_with('0')) {
        k.to_string()
    } else {
        quote(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        for s in ["a", "a\"b", "x\\y", "tab\there", "nl\nx", "\u{1}", "{=x}", "é ×"] {
            let q = quote(s);
            assert_eq!(string_value(&q, 0), s, "{q}");
        }
    }

    #[test]
    fn blocks() {
        for s in ["## A\nb", "\nlead", "trail\n", "a\n\n  indented\nq\"\"\"q", "x\\y\n\"\""] {
            let b = text_block(s, 8);
            let tok = b.trim_start();
            assert_eq!(string_value(tok, 8), s, "{b}");
        }
    }

    #[test]
    fn data_literals() {
        assert_eq!(data(&serde_json::json!([1, "Confirmed", "2026-01-01", "a b", null, true, {"k": "v", "x y": 2.5}])), "[1, Confirmed, 2026-01-01, \"a b\", none, true, {k: v, \"x y\": 2.5}]");
    }
}
