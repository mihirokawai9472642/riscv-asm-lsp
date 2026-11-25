use tree_sitter::Tree;

use crate::position::clamp_to_char_boundary;
use crate::preprocessor::{HashLineKind, classify_hash_line};

fn for_each_unquoted(s: &str, mut f: impl FnMut(usize, char) -> bool) {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.char_indices();
    while let Some((idx, ch)) = chars.next() {
        if in_single {
            if ch == '\\' {
                chars.next();
            } else if ch == '\'' {
                in_single = false;
            }
        } else if in_double {
            if ch == '\\' {
                chars.next();
            } else if ch == '"' {
                in_double = false;
            }
        } else {
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                _ => {}
            }
            if !f(idx, ch) {
                break;
            }
        }
    }
}

/// Strip `// ...`, `# ...`, and block comments `/* ... */` outside quotes on a single line.
/// Closed block comments are replaced by spaces of equal byte length so byte offsets are preserved;
/// unclosed `/*` or line comments truncate the remainder of the line.
pub fn strip_comments(line: &str) -> String {
    let mut in_block = false;
    strip_comments_with_state(line, &mut in_block, false)
}

/// Strip comments on a single line while tracking multi-line `/* ... */` state across lines.
///
/// Replaces skipped block-comment spans with equal-byte spaces so remaining byte offsets match
/// `line`. When `preprocessed` is true, a leading `#<directive>` line does not act as a line
/// comment, so `/*` on a `#define` line properly updates `in_block_comment`.
pub fn strip_comments_with_state(
    line: &str,
    in_block_comment: &mut bool,
    preprocessed: bool,
) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut only_ws = true;

    if *in_block_comment {
        if let Some(rel_end) = line.find("*/") {
            let skip = rel_end + 2;
            out.extend(std::iter::repeat_n(' ', skip));
            i = skip;
            *in_block_comment = false;
            only_ws = false;
        } else {
            return String::new();
        }
    }

    let is_hash_directive = preprocessed
        && matches!(
            classify_hash_line(line),
            Some(HashLineKind::Directive { .. })
        );

    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            let ch_len = utf8_char_len(bytes, i);
            out.push_str(&line[i..i + ch_len]);
            if b == b'\\' && i + 1 < bytes.len() {
                let next_len = utf8_char_len(bytes, i + 1);
                out.push_str(&line[i + 1..i + 1 + next_len]);
                i += 1 + next_len;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += ch_len;
        } else if in_double {
            let ch_len = utf8_char_len(bytes, i);
            out.push_str(&line[i..i + ch_len]);
            if b == b'\\' && i + 1 < bytes.len() {
                let next_len = utf8_char_len(bytes, i + 1);
                out.push_str(&line[i + 1..i + 1 + next_len]);
                i += 1 + next_len;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += ch_len;
        } else if b == b' ' || b == b'\t' || b == b'\r' {
            out.push(b as char);
            i += 1;
        } else if b == b'\'' {
            in_single = true;
            only_ws = false;
            out.push('\'');
            i += 1;
        } else if b == b'"' {
            in_double = true;
            only_ws = false;
            out.push('"');
            i += 1;
        } else if b == b'#' {
            if only_ws && is_hash_directive {
                only_ws = false;
                out.push('#');
                i += 1;
            } else {
                break;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            only_ws = false;
            if let Some(rel_end) = line[i + 2..].find("*/") {
                let comment_len = 2 + rel_end + 2;
                out.extend(std::iter::repeat_n(' ', comment_len));
                i += comment_len;
            } else {
                *in_block_comment = true;
                break;
            }
        } else {
            only_ws = false;
            let ch_len = utf8_char_len(bytes, i);
            out.push_str(&line[i..i + ch_len]);
            i += ch_len;
        }
    }

    out
}

/// Return `true` when `offset` lies inside a comment (`//`, `#`, or `/* ... */`).
///
/// Checks the tree-sitter `comment` node at the cursor and falls back to scanning from file
/// start so unclosed `/*` and multi-line block comments are handled reliably.
pub fn is_offset_in_comment(
    text: &str,
    tree: Option<&Tree>,
    offset: usize,
    preprocessed: bool,
) -> bool {
    let clamped = clamp_to_char_boundary(text, offset.min(text.len()));
    if let Some(tree) = tree
        && tree_node_is_comment(text, tree, clamped, preprocessed)
    {
        return true;
    }
    let (in_block, in_line) = scan_comment_state(text, clamped, preprocessed);
    in_block || in_line
}

/// Return `true` if `offset` starts inside an open multi-line `/* ... */` block comment.
pub fn in_block_comment_before(text: &str, offset: usize, preprocessed: bool) -> bool {
    let clamped = clamp_to_char_boundary(text, offset.min(text.len()));
    scan_comment_state(text, clamped, preprocessed).0
}

fn tree_node_is_comment(text: &str, tree: &Tree, offset: usize, preprocessed: bool) -> bool {
    let root = tree.root_node();
    for probe in [Some(offset), offset.checked_sub(1)].into_iter().flatten() {
        if text.is_empty() || probe >= text.len() {
            continue;
        }
        let Some(node) = root.descendant_for_byte_range(probe, probe) else {
            continue;
        };
        if node.kind() != "comment" {
            continue;
        }
        let Ok(raw) = node.utf8_text(text.as_bytes()) else {
            continue;
        };
        if preprocessed
            && matches!(
                classify_hash_line(raw),
                Some(HashLineKind::Directive { .. })
            )
        {
            continue;
        }
        let start = node.start_byte();
        let end = node.end_byte();
        if raw.starts_with("/*") && raw.ends_with("*/") && raw.len() >= 4 {
            if offset > start && offset < end {
                return true;
            }
        } else if ((offset > start && offset <= end)
            || (probe == offset && offset == start && raw.starts_with('#')))
            && !(offset == end && raw.ends_with('\n'))
        {
            return true;
        }
    }
    false
}

fn scan_comment_state(text: &str, offset: usize, preprocessed: bool) -> (bool, bool) {
    let bytes = text.as_bytes();
    let limit = offset.min(bytes.len());
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut only_ws_on_line = true;

    while i < limit {
        let b = bytes[i];
        if b == b'\n' {
            in_single = false;
            in_double = false;
            in_line_comment = false;
            only_ws_on_line = true;
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                if i + 2 <= limit {
                    in_block_comment = false;
                    only_ws_on_line = false;
                    i += 2;
                    continue;
                }
                return (true, false);
            }
            i += 1;
            continue;
        }
        if in_line_comment {
            i += 1;
            continue;
        }
        if in_single || in_double {
            let quote = if in_single { b'\'' } else { b'"' };
            if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] != b'\n' {
                i += 2;
            } else {
                if b == quote {
                    in_single = false;
                    in_double = false;
                }
                i += 1;
            }
            continue;
        }

        match b {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\'' => {
                in_single = true;
                only_ws_on_line = false;
                i += 1;
            }
            b'"' => {
                in_double = true;
                only_ws_on_line = false;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block_comment = true;
                only_ws_on_line = false;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                in_line_comment = true;
                only_ws_on_line = false;
                i += 2;
            }
            b'#' => {
                if preprocessed
                    && only_ws_on_line
                    && let Some(line_slice) = line_from(text, i)
                    && matches!(
                        classify_hash_line(line_slice),
                        Some(HashLineKind::Directive { .. })
                    )
                {
                    only_ws_on_line = false;
                    i += 1;
                    continue;
                }
                in_line_comment = true;
                only_ws_on_line = false;
                i += 1;
            }
            _ => {
                only_ws_on_line = false;
                i += 1;
            }
        }
    }

    (in_block_comment, in_line_comment)
}

fn line_from(text: &str, start: usize) -> Option<&str> {
    let slice = text.get(start..)?;
    let end = slice.find('\n').unwrap_or(slice.len());
    Some(&slice[..end])
}

/// Split `s` at the last unquoted `;`, returning the active statement tail.
pub fn last_unquoted_statement(s: &str) -> &str {
    let mut last_semi = 0;
    for_each_unquoted(s, |idx, ch| {
        if ch == ';' {
            last_semi = idx + 1;
        }
        true
    });
    &s[last_semi..]
}

/// Strip one or more leading assembly label prefixes (`foo:`, `1:`) from a statement.
pub fn strip_leading_labels(stmt: &str) -> &str {
    let mut rest = stmt.trim_start();
    loop {
        let mut colon_pos = None;
        for (idx, ch) in rest.char_indices() {
            if ch == ':' {
                colon_pos = Some(idx);
                break;
            }
            if !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | '.' | '$') && !ch.is_whitespace()
            {
                break;
            }
        }
        let Some(pos) = colon_pos else {
            return rest;
        };
        let label = rest[..pos].trim();
        if label.is_empty() || label.contains(char::is_whitespace) {
            return rest;
        }
        rest = rest[pos + 1..].trim_start();
    }
}

/// Return the active partial statement prefix after stripping comments, prior `;` statements,
/// and leading labels.
pub fn active_statement_prefix(line_prefix: &str) -> String {
    active_statement_prefix_with_block_state(line_prefix, false)
}

/// Variant of `active_statement_prefix` that tracks whether `line_prefix` starts inside
/// a multi-line `/* ... */` block comment.
pub fn active_statement_prefix_with_block_state(
    line_prefix: &str,
    starts_in_block: bool,
) -> String {
    let mut in_block = starts_in_block;
    let stripped = strip_comments_with_state(line_prefix, &mut in_block, false);
    if in_block {
        return String::new();
    }
    let stmt = last_unquoted_statement(&stripped);
    strip_leading_labels(stmt).trim_start().to_owned()
}

/// Return `true` when `line_prefix` places the token at statement head (no preceding opcode).
pub fn is_statement_head(line_prefix: &str) -> bool {
    is_statement_head_with_block_state(line_prefix, false)
}

/// Variant of `is_statement_head` that accounts for a multi-line `/* ... */` block comment
/// closed earlier on the same line.
pub fn is_statement_head_with_block_state(line_prefix: &str, starts_in_block: bool) -> bool {
    active_statement_prefix_with_block_state(line_prefix, starts_in_block).is_empty()
}

/// Count unquoted commas at paren depth 0 in `args` and whether depth > 0 at EOF.
pub fn count_operand_position(args: &str) -> (usize, bool) {
    let stripped = strip_comments(args);
    let mut depth = 0usize;
    let mut idx = 0usize;
    for_each_unquoted(&stripped, |_, ch| {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => idx += 1,
            _ => {}
        }
        true
    });
    (idx, depth > 0)
}

/// Extract the statement on `full_line` around `rel_col` (byte column), stripped of comments and labels.
pub fn extract_statement_at(full_line: &str, rel_col: usize) -> Option<String> {
    extract_statement_at_with_block_state(full_line, rel_col, false)
}

/// Variant of `extract_statement_at` that tracks whether `full_line` starts inside a multi-line
/// `/* ... */` block comment.
pub fn extract_statement_at_with_block_state(
    full_line: &str,
    rel_col: usize,
    starts_in_block: bool,
) -> Option<String> {
    let mut in_block = starts_in_block;
    let without_comment = strip_comments_with_state(full_line, &mut in_block, false);
    let mut seg_start = 0usize;
    let mut selected = without_comment.as_str();
    let mut found = false;
    for_each_unquoted(&without_comment, |idx, ch| {
        if ch == ';' {
            if rel_col >= seg_start && rel_col <= idx {
                selected = &without_comment[seg_start..idx];
                found = true;
                return false;
            }
            seg_start = idx + 1;
        }
        true
    });
    if !found && rel_col >= seg_start {
        selected = &without_comment[seg_start..];
    }
    let clean = strip_leading_labels(selected).trim();
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_owned())
    }
}

/// Split `line` on unquoted `;` separators after stripping comments.
pub fn split_unquoted_statements(line: &str) -> Vec<String> {
    let stripped = strip_comments(line);
    split_unquoted_statements_stripped(&stripped)
}

/// Split an already comment-stripped line on unquoted `;` separators.
pub fn split_unquoted_statements_stripped(stripped: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for_each_unquoted(stripped, |idx, ch| {
        if ch == ';' {
            out.push(stripped[start..idx].to_owned());
            start = idx + 1;
        }
        true
    });
    out.push(stripped[start..].to_owned());
    out
}

fn utf8_char_len(bytes: &[u8], idx: usize) -> usize {
    let b = bytes[idx];
    if b < 0x80 {
        1
    } else if (b & 0xE0) == 0xC0 {
        2.min(bytes.len() - idx)
    } else if (b & 0xF0) == 0xE0 {
        3.min(bytes.len() - idx)
    } else {
        4.min(bytes.len() - idx)
    }
}
