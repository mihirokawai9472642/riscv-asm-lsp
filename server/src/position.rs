use lsp_types::{Position, Range};

/// Per-line byte offset index for safe conversion between LSP UTF-16 positions,
/// clang 1-based byte columns, and UTF-8 byte offsets.
#[derive(Debug, Clone)]
pub struct LineIndex {
    line_starts: Vec<usize>,
    text_len: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self {
            line_starts,
            text_len: text.len(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Return `(line_start, content_end, raw_line_end)` for a 0-based line index.
    /// `content_end` excludes trailing `\n` or `\r\n`.
    pub fn line_bounds(&self, text: &str, line: usize) -> (usize, usize, usize) {
        if self.line_starts.is_empty() || text.is_empty() {
            return (0, 0, 0);
        }
        let clamped_line = line.min(self.line_starts.len().saturating_sub(1));
        let start = self
            .line_starts
            .get(clamped_line)
            .copied()
            .unwrap_or(0)
            .min(text.len());
        let raw_end = self
            .line_starts
            .get(clamped_line + 1)
            .copied()
            .unwrap_or(text.len())
            .min(text.len());
        let line_slice = text.get(start..raw_end).unwrap_or("");
        let stripped = line_slice.strip_suffix('\n').unwrap_or(line_slice);
        let stripped = stripped.strip_suffix('\r').unwrap_or(stripped);
        let content_end = start + stripped.len();
        (start, content_end, raw_end)
    }

    /// Convert an LSP UTF-16 `Position` to a valid UTF-8 byte offset in `text`,
    /// clamping out-of-range lines/columns and snapping surrogate-split positions
    /// safely to character boundaries.
    pub fn position_to_offset(&self, text: &str, pos: Position) -> usize {
        if text.is_empty() || self.line_starts.is_empty() {
            return 0;
        }
        let target_line = pos.line as usize;
        if target_line >= self.line_starts.len() {
            return text.len();
        }
        let (start, content_end, _) = self.line_bounds(text, target_line);
        let content = text.get(start..content_end).unwrap_or("");

        let mut utf16_acc: u32 = 0;
        for (rel_byte, ch) in content.char_indices() {
            let width = ch.len_utf16() as u32;
            if utf16_acc == pos.character || utf16_acc + width > pos.character {
                return start + rel_byte;
            }
            utf16_acc += width;
        }
        content_end
    }

    /// Convert a byte offset into an LSP UTF-16 `Position`, clamping to `text` bounds
    /// and snapping to a UTF-8 character boundary.
    pub fn offset_to_position(&self, text: &str, offset: usize) -> Position {
        if text.is_empty() || self.line_starts.is_empty() {
            return Position::new(0, 0);
        }
        let clamped = clamp_to_char_boundary(text, offset.min(self.text_len).min(text.len()));
        let line_idx = match self.line_starts.binary_search(&clamped) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let (start, content_end, _) = self.line_bounds(text, line_idx);
        let safe_end = clamped.clamp(start, content_end);
        let prefix = text.get(start..safe_end).unwrap_or("");
        let utf16_col: u32 = prefix.chars().map(|ch| ch.len_utf16() as u32).sum();
        Position::new(line_idx as u32, utf16_col)
    }

    pub fn byte_span_to_range(&self, text: &str, start: usize, end: usize) -> Range {
        let (s, e) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Range::new(
            self.offset_to_position(text, s),
            self.offset_to_position(text, e),
        )
    }

    /// Convert clang's 1-based line and 1-based byte column into a valid UTF-8 byte offset.
    pub fn clang_pos_to_offset(&self, text: &str, line_1based: u32, byte_col_1based: u32) -> usize {
        if text.is_empty() || self.line_starts.is_empty() {
            return 0;
        }
        let line_idx = line_1based.saturating_sub(1) as usize;
        if line_idx >= self.line_starts.len() {
            return text.len();
        }
        let (start, content_end, _) = self.line_bounds(text, line_idx);
        let col_bytes = byte_col_1based.saturating_sub(1) as usize;
        let raw = (start + col_bytes).min(content_end);
        clamp_to_char_boundary(text, raw)
    }
}

/// Clamp a byte offset so it lies on a valid UTF-8 character boundary within `text`.
pub fn clamp_to_char_boundary(text: &str, offset: usize) -> usize {
    let mut pos = offset.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Find the byte span `(start, end)` of an assembly token around `offset`.
/// Includes identifiers, directives (`.equ`), registers, numeric labels (`1f`),
/// macro parameter references (`\reg`), and relocation hints (`%hi`).
pub fn word_range_at(text: &str, offset: usize) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    let pos = clamp_to_char_boundary(text, offset);
    let bytes = text.as_bytes();

    // If cursor sits right after an identifier char (e.g. end of word), peek left.
    let probe = if pos < bytes.len() && is_word_byte(bytes[pos]) {
        pos
    } else if pos > 0 && is_word_byte(bytes[pos - 1]) {
        pos - 1
    } else {
        return (pos, pos);
    };

    let mut start = probe;
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    // Include a leading sigil like `.` (directive/local label), `%` (relocation),
    // or `\` (macro parameter) if directly attached.
    if start > 0 && matches!(bytes[start - 1], b'.' | b'%' | b'\\') {
        start -= 1;
    }

    let mut end = probe + 1;
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }

    (
        clamp_to_char_boundary(text, start),
        clamp_to_char_boundary(text, end),
    )
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_multibyte_roundtrip() {
        let text = "# 日本語コメント 🚀\naddi a0, zero, 1\n";
        let idx = LineIndex::new(text);

        // Line 0: "# " (2) + "日本語コメント" (7 chars * 3 bytes = 21 bytes, 7 UTF-16 units)
        // + " " (1) + "🚀" (4 bytes, 2 UTF-16 units)
        let rocket_pos = Position::new(0, 10);
        let rocket_offset = idx.position_to_offset(text, rocket_pos);
        assert_eq!(&text[rocket_offset..rocket_offset + 4], "🚀");
        assert_eq!(idx.offset_to_position(text, rocket_offset), rocket_pos);

        // Middle of surrogate pair snaps to start of 🚀
        let mid_rocket = idx.position_to_offset(text, Position::new(0, 11));
        assert_eq!(mid_rocket, rocket_offset);

        // End of line 0 (after 🚀)
        let eol_offset = idx.position_to_offset(text, Position::new(0, 12));
        assert_eq!(eol_offset, "# 日本語コメント 🚀".len());
        assert_eq!(
            idx.offset_to_position(text, eol_offset),
            Position::new(0, 12)
        );
    }

    #[test]
    fn out_of_range_positions_clamp_safely() {
        let text = "nop\n\nret";
        let idx = LineIndex::new(text);

        // Column far past line end
        assert_eq!(idx.position_to_offset(text, Position::new(0, 999)), 3);
        // Empty middle line
        assert_eq!(idx.position_to_offset(text, Position::new(1, 0)), 4);
        assert_eq!(idx.position_to_offset(text, Position::new(1, 42)), 4);
        // Line far past EOF
        assert_eq!(
            idx.position_to_offset(text, Position::new(100, 50)),
            text.len()
        );
        // Empty document
        let empty = "";
        let empty_idx = LineIndex::new(empty);
        assert_eq!(empty_idx.position_to_offset(empty, Position::new(5, 5)), 0);
        assert_eq!(empty_idx.offset_to_position(empty, 99), Position::new(0, 0));
    }

    #[test]
    fn crlf_line_endings_clamp_before_cr() {
        let text = "mv a0, a1\r\nret\r\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position_to_offset(text, Position::new(0, 99)), 9);
        assert_eq!(idx.position_to_offset(text, Position::new(1, 0)), 11);
    }

    #[test]
    fn clang_byte_column_handles_multibyte_prefix() {
        let text = "/* 日本語 */ mvi a0, 1\n";
        let idx = LineIndex::new(text);
        // "/* 日本語 */ " is 3 + 9 + 4 = 16 bytes, so `mvi` starts at 1-based byte col 17.
        let byte_col = "/* 日本語 */ ".len() as u32 + 1;
        let offset = idx.clang_pos_to_offset(text, 1, byte_col);
        assert_eq!(&text[offset..offset + 3], "mvi");
        let (ws, we) = word_range_at(text, offset);
        assert_eq!(&text[ws..we], "mvi");

        // Mid-UTF8 column from external tool snaps cleanly to char boundary.
        let mid_utf8 = idx.clang_pos_to_offset(text, 1, 5);
        assert!(text.is_char_boundary(mid_utf8));
    }
}
