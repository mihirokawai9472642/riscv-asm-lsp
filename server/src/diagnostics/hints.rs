use std::collections::{HashMap, HashSet};
use std::path::Path;

use lsp_types::{DiagnosticRelatedInformation, Location, Range, Uri};

use crate::position::LineIndex;
use crate::preprocessor::{classify_hash_line, parse_cpp_define_line};

use super::MacroDefHint;
use super::parse::trimmed_line_range;

pub(super) struct CppDefineLine {
    pub name: String,
    pub def_line: u32,
    pub value: Option<String>,
    pub is_function_like: bool,
}

pub(super) struct MacroHintQuery<'a> {
    pub line_index: &'a LineIndex,
    pub text: &'a str,
    pub diag_line: u32,
    pub diag_col_1based: u32,
    pub diag_message: &'a str,
    pub defined_macros: &'a [CppDefineLine],
    pub doc_uri: Option<&'a Uri>,
    pub doc_path: Option<&'a Path>,
}

pub(super) fn collect_cpp_define_lines(text: &str) -> Vec<CppDefineLine> {
    let mut defs = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if let Some(parsed) = parse_cpp_define_line(line) {
            defs.push(CppDefineLine {
                name: parsed.name.to_owned(),
                def_line: idx as u32,
                value: parsed.value,
                is_function_like: parsed.is_function_like,
            });
        }
    }
    defs
}

pub(super) fn build_macro_def_hints(
    q: &MacroHintQuery<'_>,
) -> (Option<Vec<DiagnosticRelatedInformation>>, Vec<MacroDefHint>) {
    let (ls, ce, _) = q.line_index.line_bounds(q.text, q.diag_line as usize);
    let raw_line = q.text.get(ls..ce).unwrap_or("");
    if classify_hash_line(raw_line).is_some() {
        return (None, Vec::new());
    }
    let code = strip_asm_comment_for_ident(raw_line);

    let mut latest_by_name: HashMap<&str, (&CppDefineLine, usize)> = HashMap::new();
    for def in q.defined_macros {
        if def.def_line < q.diag_line
            && let Some(ref_col) = find_ident_word_col(code, &def.name)
        {
            latest_by_name.insert(def.name.as_str(), (def, ref_col));
        }
    }
    if latest_by_name.is_empty() {
        return (None, Vec::new());
    }

    let mut matched: Vec<(&CppDefineLine, usize)> = latest_by_name.into_values().collect();
    matched.sort_by(|a, b| {
        a.0.def_line
            .cmp(&b.0.def_line)
            .then_with(|| a.0.name.cmp(&b.0.name))
    });

    let file_kind = if q
        .doc_path
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        == Some("S")
    {
        "`.S` files"
    } else {
        "preprocessed files"
    };
    let error_matches_expansion = is_macro_expansion_error_message(q.diag_message);

    let mut hints = Vec::new();
    let mut related = Vec::with_capacity(matched.len());
    for (def, ref_col) in matched {
        let col_in_expansion = q.diag_col_1based >= (ref_col as u32).saturating_add(1);
        let suggest_comment_fix = error_matches_expansion
            && col_in_expansion
            && looks_like_prose_define(def.value.as_deref(), def.is_function_like);

        if let Some(uri) = q.doc_uri {
            let def_range = trimmed_line_range(q.line_index, q.text, def.def_line as usize);
            let message = format_macro_hint_message(def, suggest_comment_fix, file_kind);
            related.push(DiagnosticRelatedInformation {
                location: Location::new(uri.clone(), def_range),
                message,
            });
        }

        if suggest_comment_fix {
            hints.push(MacroDefHint {
                macro_name: def.name.clone(),
                def_line: def.def_line,
            });
        }
    }

    let rel_opt = (!related.is_empty()).then_some(related);
    (rel_opt, hints)
}

fn format_macro_hint_message(
    def: &CppDefineLine,
    suggest_comment_fix: bool,
    file_kind: &str,
) -> String {
    let line_num = def.def_line + 1;
    if suggest_comment_fix {
        match &def.value {
            Some(val) => format!(
                "`{}` expands to `{val}` (defined by the preprocessor directive on line {line_num}); in {file_kind} a `#` line starting with a directive name is not a comment",
                def.name
            ),
            None => format!(
                "`{}` is defined by the preprocessor directive on line {line_num}; in {file_kind} a `#` line starting with a directive name is not a comment",
                def.name
            ),
        }
    } else {
        match &def.value {
            Some(val) => format!(
                "`{}` expands to `{val}` (defined on line {line_num})",
                def.name
            ),
            None => format!("`{}` is defined on line {line_num}", def.name),
        }
    }
}

fn is_macro_expansion_error_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("unexpected token")
        || lower.contains("unknown token in expression")
        || lower.contains("invalid operand")
        || lower.contains("unrecognized instruction mnemonic")
}

fn looks_like_prose_define(value: Option<&str>, is_function_like: bool) -> bool {
    if is_function_like {
        return false;
    }
    let Some(val) = value else {
        return false;
    };
    if val.chars().any(|c| {
        matches!(
            c,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '+'
                | '-'
                | '*'
                | '/'
                | '%'
                | '&'
                | '|'
                | '^'
                | '~'
                | '<'
                | '>'
                | '='
                | '!'
                | '?'
                | '\\'
        )
    }) {
        return false;
    }
    let mut cur_run = 0usize;
    let mut max_run = 0usize;
    for raw_tok in val.split_whitespace() {
        let tok = raw_tok.trim_end_matches([',', '.']);
        let is_plain_word = !tok.is_empty()
            && tok.bytes().all(|b| b.is_ascii_alphabetic())
            && tok.bytes().any(|b| b.is_ascii_lowercase());
        if is_plain_word {
            cur_run += 1;
            max_run = max_run.max(cur_run);
        } else {
            cur_run = 0;
        }
    }
    max_run >= 2
}

fn strip_asm_comment_for_ident(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if b == b'\'' {
            in_single = true;
        } else if b == b'"' {
            in_double = true;
        } else if b == b'#' || (b == b'/' && bytes.get(i + 1) == Some(&b'/')) {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn find_ident_word_col(haystack: &str, ident: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut i = 0;
    while let Some(pos) = haystack[i..].find(ident) {
        let start = i + pos;
        let end = start + ident.len();
        let before_ok =
            start == 0 || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_');
        let after_ok =
            end >= bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
        if before_ok && after_ok {
            return Some(start);
        }
        i = start + 1;
    }
    None
}

pub(super) fn extract_suggestions(message: &str) -> Vec<String> {
    let Some(idx) = message.find("did you mean:") else {
        return Vec::new();
    };
    let rest = &message[idx + "did you mean:".len()..];
    let body = rest.split('?').next().unwrap_or(rest);
    body.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn insert_dedup_key(
    seen: &mut HashSet<(u32, u32, u32, u32, String)>,
    range: Range,
    message: &str,
) -> bool {
    seen.insert((
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
        message.to_owned(),
    ))
}
