//! Conversion between byte offsets and LSP positions (line, UTF-16 column).

use quadra_syntax::{TextRange, TextSize};
use tower_lsp::lsp_types::{Position, Range};

pub struct LineIndex<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> LineIndex<'a> {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { text, line_starts }
    }

    pub fn position(&self, offset: TextSize) -> Position {
        let offset = usize::from(offset).min(self.text.len());
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let start = self.line_starts[line];
        let col: usize = self.text[start..offset].chars().map(char::len_utf16).sum();
        Position::new(line as u32, col as u32)
    }

    pub fn range(&self, range: TextRange) -> Range {
        Range::new(self.position(range.start()), self.position(range.end()))
    }

    pub fn offset(&self, pos: Position) -> TextSize {
        let line = (pos.line as usize).min(self.line_starts.len() - 1);
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let mut utf16 = 0usize;
        let mut offset = start;
        for ch in self.text[start..end].chars() {
            if utf16 >= pos.character as usize || ch == '\n' {
                break;
            }
            utf16 += ch.len_utf16();
            offset += ch.len_utf8();
        }
        TextSize::from(offset as u32)
    }

    /// Start offset of each line; used by semantic tokens.
    pub fn line_col(&self, offset: TextSize) -> (u32, u32) {
        let p = self.position(offset);
        (p.line, p.character)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_non_ascii() {
        let text = "a € b\nsecond\n";
        let idx = LineIndex::new(text);
        let off = TextSize::from(text.find('b').unwrap() as u32);
        let pos = idx.position(off);
        assert_eq!(pos, Position::new(0, 4));
        assert_eq!(idx.offset(pos), off);
        assert_eq!(idx.position(TextSize::from(8)), Position::new(1, 0));
    }
}
