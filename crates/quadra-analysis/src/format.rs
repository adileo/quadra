//! `quadra fmt`: conservative formatting that never changes meaning.
//!
//! It only touches whitespace that the grammar ignores: trailing spaces, runs of blank
//! lines (at most two), and the final newline. Indentation is left alone because it is
//! significant; the parser already rejects tabs and inconsistent dedents.

pub fn format_source(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    let mut in_triple = false;
    for line in text.lines() {
        let trimmed = if in_triple { line } else { line.trim_end() };
        if line.matches("\"\"\"").count() % 2 == 1 {
            in_triple = !in_triple;
        }
        if trimmed.trim().is_empty() && !in_triple {
            blank += 1;
            if blank > 2 {
                continue;
            }
            out.push('\n');
            continue;
        }
        blank = 0;
        out.push_str(trimmed);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_only() {
        let src = "entity A:   \n    x: Int\n\n\n\n\nentity B:\n    y: Int\n\n\n";
        assert_eq!(format_source(src), "entity A:\n    x: Int\n\n\nentity B:\n    y: Int\n");
        let formatted = format_source(src);
        assert_eq!(format_source(&formatted), formatted, "idempotent");
    }
}
