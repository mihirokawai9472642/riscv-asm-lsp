use crate::data::texts_data;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashLineKind<'a> {
    Directive {
        hash_offset: usize,
        name: &'a str,
        name_span: (usize, usize),
        rest: &'a str,
    },
    LineMarker {
        hash_offset: usize,
        num_span: (usize, usize),
    },
    NullDirective {
        hash_offset: usize,
    },
    Comment {
        hash_offset: usize,
    },
}

impl HashLineKind<'_> {
    pub fn hash_offset(&self) -> usize {
        match *self {
            Self::Directive { hash_offset, .. }
            | Self::LineMarker { hash_offset, .. }
            | Self::NullDirective { hash_offset }
            | Self::Comment { hash_offset } => hash_offset,
        }
    }

    pub fn is_directive_or_marker(&self) -> bool {
        matches!(
            self,
            Self::Directive { .. } | Self::LineMarker { .. } | Self::NullDirective { .. }
        )
    }
}

pub fn is_cpp_directive(name: &str) -> bool {
    texts_data()
        .preprocessor_directives
        .iter()
        .any(|d| d.name == name)
}

pub fn directive_description(name: &str) -> Option<&'static str> {
    texts_data()
        .preprocessor_directives
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.description.as_str())
}

/// Classify a single line whose first non-blank character is `#`.
///
/// Returns `None` when `#` is preceded by non-whitespace tokens (for example
/// `nop # if define error`, where `#` begins a trailing assembly comment).
pub fn classify_hash_line(line: &str) -> Option<HashLineKind<'_>> {
    let bytes = line.as_bytes();
    let content_end = line.trim_end_matches(['\r', '\n']).len();

    let mut hash_offset = 0usize;
    while hash_offset < content_end && matches!(bytes[hash_offset], b' ' | b'\t') {
        hash_offset += 1;
    }
    if hash_offset >= content_end || bytes[hash_offset] != b'#' {
        return None;
    }

    let mut pos = hash_offset + 1;
    while pos < content_end && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    if pos >= content_end {
        return Some(HashLineKind::NullDirective { hash_offset });
    }

    let b = bytes[pos];
    if b.is_ascii_digit() {
        let mut end = pos + 1;
        while end < content_end && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let next_is_ident =
            end < content_end && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        if !next_is_ident {
            return Some(HashLineKind::LineMarker {
                hash_offset,
                num_span: (pos, end),
            });
        }
        return Some(HashLineKind::Comment { hash_offset });
    }

    if b.is_ascii_alphabetic() || b == b'_' {
        let mut end = pos + 1;
        while end < content_end && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let word = &line[pos..end];
        if is_cpp_directive(word) {
            return Some(HashLineKind::Directive {
                hash_offset,
                name: word,
                name_span: (pos, end),
                rest: &line[end..content_end],
            });
        }
    }

    Some(HashLineKind::Comment { hash_offset })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDefine<'a> {
    pub name: &'a str,
    pub name_span: (usize, usize),
    pub value: Option<String>,
    pub is_function_like: bool,
}

pub fn parse_cpp_define_line(line: &str) -> Option<ParsedDefine<'_>> {
    let Some(HashLineKind::Directive {
        name: "define",
        name_span: dir_span,
        rest,
        ..
    }) = classify_hash_line(line)
    else {
        return None;
    };

    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let after_ws = rest.trim_start_matches([' ', '\t']);
    let ws_len = rest.len() - after_ws.len();
    let ident_start = dir_span.1 + ws_len;

    let bytes = after_ws.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut ident_len = 1usize;
    while ident_len < bytes.len()
        && (bytes[ident_len].is_ascii_alphanumeric() || bytes[ident_len] == b'_')
    {
        ident_len += 1;
    }
    let ident = &after_ws[..ident_len];
    let after_ident = &after_ws[ident_len..];
    if !(after_ident.is_empty()
        || after_ident.starts_with([' ', '\t'])
        || after_ident.starts_with('('))
    {
        return None;
    }

    let is_function_like = after_ident.starts_with('(');
    let raw_val = if is_function_like {
        after_ident.trim().to_owned()
    } else {
        strip_cpp_line_comment(after_ident).trim().to_owned()
    };
    let value = (!raw_val.is_empty()).then_some(raw_val);

    Some(ParsedDefine {
        name: ident,
        name_span: (ident_start, ident_start + ident_len),
        value,
        is_function_like,
    })
}

/// Strip C/C++ comments (`//` and `/* ... */`) from a `#define` value while respecting
/// quotes and preserving `#`/`##` preprocessor operators.
pub fn strip_cpp_line_comment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if in_single {
            out.push(ch);
            if ch == '\\' {
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            } else if ch == '\'' {
                in_single = false;
            }
        } else if in_double {
            out.push(ch);
            if ch == '\\' {
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            } else if ch == '"' {
                in_double = false;
            }
        } else if ch == '\'' {
            in_single = true;
            out.push(ch);
        } else if ch == '"' {
            in_double = true;
            out.push(ch);
        } else if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '/') {
            break;
        } else if ch == '/' && chars.peek().is_some_and(|&(_, next)| next == '*') {
            chars.next();
            let rest = &s[idx + 2..];
            if let Some(rel_end) = rest.find("*/") {
                out.push(' ');
                let end_byte = idx + 2 + rel_end + 2;
                while let Some(&(next_idx, _)) = chars.peek() {
                    if next_idx < end_byte {
                        chars.next();
                    } else {
                        break;
                    }
                }
            } else {
                break;
            }
        } else {
            out.push(ch);
        }
    }

    out
}

pub fn parse_cpp_include_line(line: &str) -> Option<(String, bool)> {
    let Some(HashLineKind::Directive { name, rest, .. }) = classify_hash_line(line) else {
        return None;
    };
    if !matches!(name, "include" | "include_next" | "import") {
        return None;
    }
    let arg = rest.trim_start_matches([' ', '\t']);
    if let Some(inner) = arg.strip_prefix('"') {
        let end = inner.find('"')?;
        return Some((inner[..end].to_owned(), false));
    }
    if let Some(inner) = arg.strip_prefix('<') {
        let end = inner.find('>')?;
        return Some((inner[..end].to_owned(), true));
    }
    None
}

pub fn is_cpp_preamble_line(line: &str) -> bool {
    matches!(
        classify_hash_line(line),
        Some(HashLineKind::Directive { name, .. })
            if matches!(
                name,
                "include"
                    | "include_next"
                    | "import"
                    | "define"
                    | "undef"
                    | "if"
                    | "ifdef"
                    | "ifndef"
                    | "elif"
                    | "elifdef"
                    | "elifndef"
                    | "else"
                    | "endif"
            )
    )
}

pub fn hash_to_slash_replacement(line: &str, hash_offset: usize) -> &'static str {
    match line.as_bytes().get(hash_offset + 1) {
        None | Some(b' ' | b'\t' | b'\r' | b'\n') => "//",
        Some(_) => "// ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_preprocessor_lines_matching_clang_19() {
        assert!(matches!(
            classify_hash_line("# just a comment"),
            Some(HashLineKind::Comment { hash_offset: 0 })
        ));
        assert!(classify_hash_line("nop # if define error").is_none());
        assert!(matches!(
            classify_hash_line("#"),
            Some(HashLineKind::NullDirective { hash_offset: 0 })
        ));
        assert!(matches!(
            classify_hash_line("   #   \n"),
            Some(HashLineKind::NullDirective { hash_offset: 3 })
        ));
        assert!(matches!(
            classify_hash_line("# 100 note"),
            Some(HashLineKind::LineMarker {
                hash_offset: 0,
                num_span: (2, 5)
            })
        ));
        assert!(matches!(
            classify_hash_line("# pragma: keep"),
            Some(HashLineKind::Directive {
                hash_offset: 0,
                name: "pragma",
                name_span: (2, 8),
                ..
            })
        ));
        assert!(matches!(
            classify_hash_line("# if the value is zero, skip"),
            Some(HashLineKind::Directive {
                name: "if",
                name_span: (2, 4),
                ..
            })
        ));
        assert!(matches!(
            classify_hash_line("    # include this later"),
            Some(HashLineKind::Directive {
                hash_offset: 4,
                name: "include",
                name_span: (6, 13),
                ..
            })
        ));
        assert!(matches!(
            classify_hash_line("# error handling starts here"),
            Some(HashLineKind::Directive { name: "error", .. })
        ));
        for dir in [
            "include_next",
            "elifdef",
            "elifndef",
            "embed",
            "ident",
            "sccs",
            "assert",
            "unassert",
            "warning",
        ] {
            let line = format!("# {dir} arg");
            assert!(
                matches!(classify_hash_line(&line), Some(HashLineKind::Directive { name, .. }) if name == dir),
                "expected directive {dir}"
            );
        }
    }

    #[test]
    fn parses_defines_including_spaced_and_substring_names() {
        let spaced = parse_cpp_define_line("# define X 5 is the size\n").expect("spaced define");
        assert_eq!(spaced.name, "X");
        assert_eq!(spaced.value.as_deref(), Some("5 is the size"));
        assert!(!spaced.is_function_like);

        let line = "#define def 1\n";
        let sub = parse_cpp_define_line(line).expect("def define");
        assert_eq!(sub.name, "def");
        assert_eq!(&line[sub.name_span.0..sub.name_span.1], "def");
        assert_eq!(sub.value.as_deref(), Some("1"));

        let func = parse_cpp_define_line("#define ADD(a, b) ((a) + (b))").expect("func macro");
        assert!(func.is_function_like);
        assert_eq!(func.value.as_deref(), Some("(a, b) ((a) + (b))"));

        assert_eq!(
            parse_cpp_include_line("  # include_next <regs.h>"),
            Some(("regs.h".to_owned(), true))
        );
        assert_eq!(hash_to_slash_replacement("#define FOO 1", 0), "// ");
        assert_eq!(hash_to_slash_replacement("# if foo", 0), "//");

        let block_then_line = parse_cpp_define_line("#define FOO 1 /* c */ // note").unwrap();
        assert_eq!(block_then_line.value.as_deref(), Some("1"));
        let quoted_url = parse_cpp_define_line("#define URL \"http://example.com\" // ok").unwrap();
        assert_eq!(quoted_url.value.as_deref(), Some("\"http://example.com\""));
        assert_eq!(
            strip_cpp_line_comment(" #x /* comment */ // trailing").trim(),
            "#x"
        );
    }
}
