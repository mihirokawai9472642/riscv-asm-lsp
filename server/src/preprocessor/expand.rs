use std::collections::{BTreeSet, HashMap};

use super::{ExpandedSpan, MacroDef};

const MAX_EXPANSION_DEPTH: usize = 64;
const MAX_EXPANDED_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    Number,
    StringLit,
    CharLit,
    Space,
    Hash,
    HashHash,
    Punct,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharOrigin {
    pub byte_offset: usize,
    pub source_line: usize,
    pub utf16_start: u32,
    pub utf16_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    pub source_line: usize,
    pub utf16_start: u32,
    pub utf16_end: u32,
}

#[derive(Debug, Clone)]
struct ExpToken {
    kind: TokenKind,
    text: String,
    origin: SourceRange,
    hide: BTreeSet<String>,
}

pub struct ExpandResult {
    pub text: String,
    pub spans: Vec<ExpandedSpan>,
    pub had_expansion: bool,
    pub has_unexpanded_macro: bool,
}

pub fn tokenize(input: &str) -> Vec<Token> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let (tok, next_i) = scan_next_token(input, bytes, i);
        out.push(tok);
        i = next_i;
    }

    out
}

fn scan_next_token(input: &str, bytes: &[u8], i: usize) -> (Token, usize) {
    let b = bytes[i];
    if b == b' ' || b == b'\t' {
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        return (
            Token {
                kind: TokenKind::Space,
                text: input[i..j].to_owned(),
            },
            j,
        );
    }

    if b == b'"' || b == b'\'' {
        let quote = b;
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                let ch_len = utf8_len(bytes, j + 1);
                j += 1 + ch_len;
                continue;
            }
            let ch_len = utf8_len(bytes, j);
            if bytes[j] == quote {
                j += 1;
                break;
            }
            j += ch_len;
        }
        return (
            Token {
                kind: if quote == b'"' {
                    TokenKind::StringLit
                } else {
                    TokenKind::CharLit
                },
                text: input[i..j].to_owned(),
            },
            j,
        );
    }

    if b == b'#' {
        if bytes.get(i + 1) == Some(&b'#') {
            return (
                Token {
                    kind: TokenKind::HashHash,
                    text: "##".to_owned(),
                },
                i + 2,
            );
        }
        return (
            Token {
                kind: TokenKind::Hash,
                text: "#".to_owned(),
            },
            i + 1,
        );
    }

    if b.is_ascii_digit() || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
        let mut j = i + 1;
        while j < bytes.len() {
            let cur = bytes[j];
            if cur.is_ascii_alphanumeric() || cur == b'_' || cur == b'.' {
                if matches!(cur, b'e' | b'E' | b'p' | b'P')
                    && matches!(bytes.get(j + 1), Some(b'+' | b'-'))
                {
                    j += 2;
                    continue;
                }
                j += 1;
            } else {
                break;
            }
        }
        return (
            Token {
                kind: TokenKind::Number,
                text: input[i..j].to_owned(),
            },
            j,
        );
    }

    if b.is_ascii_alphabetic() || b == b'_' {
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        return (
            Token {
                kind: TokenKind::Ident,
                text: input[i..j].to_owned(),
            },
            j,
        );
    }

    let ch_len = utf8_len(bytes, i);
    (
        Token {
            kind: TokenKind::Punct,
            text: input[i..i + ch_len].to_owned(),
        },
        i + ch_len,
    )
}

fn utf8_len(bytes: &[u8], i: usize) -> usize {
    let Some(&first) = bytes.get(i) else {
        return 0;
    };
    let expected = if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    };
    expected.min(bytes.len() - i)
}

fn source_range_for_bytes(origins: &[CharOrigin], start: usize, end: usize) -> SourceRange {
    if origins.is_empty() {
        return SourceRange {
            source_line: 0,
            utf16_start: 0,
            utf16_end: 0,
        };
    }
    let idx = origins.partition_point(|o| o.byte_offset < start);
    let Some(&first) = origins.get(idx).or_else(|| origins.last()) else {
        return SourceRange {
            source_line: 0,
            utf16_start: 0,
            utf16_end: 0,
        };
    };
    let mut utf16_end = first.utf16_end;
    for o in origins.iter().skip(idx) {
        if o.byte_offset >= end {
            break;
        }
        if o.source_line == first.source_line {
            utf16_end = o.utf16_end;
        } else {
            break;
        }
    }
    SourceRange {
        source_line: first.source_line,
        utf16_start: first.utf16_start,
        utf16_end,
    }
}

pub fn expand_logical_line(
    text: &str,
    origins: &[CharOrigin],
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
) -> ExpandResult {
    let bytes = text.as_bytes();
    let mut initial = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let (tok, next_i) = scan_next_token(text, bytes, i);
        let origin = source_range_for_bytes(origins, i, next_i);
        initial.push(ExpToken {
            kind: tok.kind,
            text: tok.text,
            origin,
            hide: BTreeSet::new(),
        });
        i = next_i;
    }

    let mut ctx = ExpandCtx {
        macros,
        has_unresolved_include,
        had_expansion: false,
        has_unexpanded_macro: false,
        uncertain_macro_used: false,
    };
    let expanded = ctx.expand_seq(&initial, 0);

    let mut out = String::new();
    let mut spans: Vec<ExpandedSpan> = Vec::new();
    for tok in expanded {
        let start = out.len();
        out.push_str(&tok.text);
        let end = out.len();
        if start < end {
            push_merged_span(&mut spans, start, end, tok.origin);
        }
        if out.len() >= MAX_EXPANDED_BYTES {
            ctx.has_unexpanded_macro = true;
            break;
        }
    }

    if !ctx.has_unexpanded_macro && has_unresolved_func_call(&out) {
        ctx.has_unexpanded_macro = true;
    }

    ExpandResult {
        text: out,
        spans,
        had_expansion: ctx.had_expansion,
        has_unexpanded_macro: ctx.has_unexpanded_macro,
    }
}

pub fn expand_string_for_expr(
    text: &str,
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
    uncertain: &mut bool,
) -> String {
    let bytes = text.as_bytes();
    let mut initial = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let (tok, next_i) = scan_next_token(text, bytes, i);
        initial.push(ExpToken {
            kind: tok.kind,
            text: tok.text,
            origin: SourceRange {
                source_line: 0,
                utf16_start: 0,
                utf16_end: 0,
            },
            hide: BTreeSet::new(),
        });
        i = next_i;
    }

    let mut ctx = ExpandCtx {
        macros,
        has_unresolved_include,
        had_expansion: false,
        has_unexpanded_macro: false,
        uncertain_macro_used: false,
    };
    let expanded = ctx.expand_seq(&initial, 0);
    if ctx.uncertain_macro_used || ctx.has_unexpanded_macro {
        *uncertain = true;
    }

    let mut out = String::new();
    for tok in expanded {
        out.push_str(&tok.text);
        if out.len() >= MAX_EXPANDED_BYTES {
            *uncertain = true;
            break;
        }
    }
    out
}

fn push_merged_span(spans: &mut Vec<ExpandedSpan>, start: usize, end: usize, origin: SourceRange) {
    if let Some(last) = spans.last_mut()
        && last.expanded.1 == start
        && last.source_line == origin.source_line
        && last.source_utf16 == (origin.utf16_start, origin.utf16_end)
    {
        last.expanded.1 = end;
        return;
    }
    spans.push(ExpandedSpan {
        expanded: (start, end),
        source_line: origin.source_line,
        source_utf16: (origin.utf16_start, origin.utf16_end),
    });
}

fn has_unresolved_func_call(expanded: &str) -> bool {
    let toks = tokenize(expanded);
    let mut i = 0usize;
    let mut at_stmt_head = true;
    while i < toks.len() {
        let tok = &toks[i];
        match tok.kind {
            TokenKind::Space => {
                i += 1;
                continue;
            }
            TokenKind::Punct if tok.text == ";" => {
                at_stmt_head = true;
                i += 1;
                continue;
            }
            TokenKind::Ident => {
                let mut j = i + 1;
                while j < toks.len() && toks[j].kind == TokenKind::Space {
                    j += 1;
                }
                if j < toks.len() && toks[j].kind == TokenKind::Punct && toks[j].text == ":" {
                    // Skip label prefix (`label:`); next token starts the statement.
                    at_stmt_head = true;
                    i = j + 1;
                    continue;
                }
                let preceded_by_percent = i > 0 && toks[i - 1].text == "%";
                if !preceded_by_percent
                    && j < toks.len()
                    && toks[j].kind == TokenKind::Punct
                    && toks[j].text == "("
                    && (at_stmt_head || has_comma_in_call(&toks[j..]))
                {
                    return true;
                }
                at_stmt_head = false;
            }
            _ => {
                at_stmt_head = false;
            }
        }
        i += 1;
    }
    false
}

fn has_comma_in_call(toks: &[Token]) -> bool {
    let mut depth = 0usize;
    for t in toks {
        if t.text == "(" {
            depth += 1;
        } else if t.text == ")" {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                break;
            }
        } else if t.text == "," && depth == 1 {
            return true;
        }
    }
    false
}

struct ExpandCtx<'a> {
    macros: &'a HashMap<String, MacroDef>,
    has_unresolved_include: bool,
    had_expansion: bool,
    has_unexpanded_macro: bool,
    uncertain_macro_used: bool,
}

impl<'a> ExpandCtx<'a> {
    fn expand_seq(&mut self, tokens: &[ExpToken], depth: usize) -> Vec<ExpToken> {
        if depth >= MAX_EXPANSION_DEPTH {
            self.has_unexpanded_macro = true;
            return tokens.to_vec();
        }

        let mut out = Vec::with_capacity(tokens.len());
        let mut total_bytes = 0usize;
        let mut i = 0usize;

        while i < tokens.len() {
            if total_bytes >= MAX_EXPANDED_BYTES {
                self.has_unexpanded_macro = true;
                out.extend_from_slice(&tokens[i..]);
                break;
            }
            let tok = &tokens[i];
            if tok.kind != TokenKind::Ident || tok.hide.contains(&tok.text) {
                total_bytes += tok.text.len();
                out.push(tok.clone());
                i += 1;
                continue;
            }

            let Some(def) = self.macros.get(&tok.text) else {
                total_bytes += tok.text.len();
                out.push(tok.clone());
                i += 1;
                continue;
            };

            if def.params.is_none() && def.variadic_param.is_none() {
                if self.has_unresolved_include && !def.certain_despite_unresolved {
                    self.uncertain_macro_used = true;
                }
                self.had_expansion = true;
                let mut child_hide = tok.hide.clone();
                child_hide.insert(tok.text.clone());
                let body_toks = tokenize_with_origin(&def.body, tok.origin, &child_hide);
                let expanded = self.expand_seq(&body_toks, depth + 1);
                for t in expanded {
                    total_bytes += t.text.len();
                    out.push(t);
                }
                i += 1;
                continue;
            }

            // Function-like macro requires `(` after optional whitespace.
            let mut j = i + 1;
            while j < tokens.len() && tokens[j].kind == TokenKind::Space {
                j += 1;
            }
            if j >= tokens.len() || tokens[j].kind != TokenKind::Punct || tokens[j].text != "(" {
                total_bytes += tok.text.len();
                out.push(tok.clone());
                i += 1;
                continue;
            }

            let Some((raw_args, close_idx)) = parse_invocation_args(tokens, j) else {
                self.has_unexpanded_macro = true;
                total_bytes += tok.text.len();
                out.push(tok.clone());
                i += 1;
                continue;
            };

            let fixed_params = def.params.as_deref().unwrap_or(&[]);
            let valid_arity = if def.variadic_param.is_some() {
                raw_args.len() >= fixed_params.len()
                    || (fixed_params.is_empty() && raw_args.len() <= 1)
            } else if fixed_params.is_empty() {
                raw_args.is_empty()
                    || (raw_args.len() == 1
                        && raw_args[0].iter().all(|t| t.kind == TokenKind::Space))
            } else {
                raw_args.len() == fixed_params.len()
            };

            if !valid_arity {
                self.has_unexpanded_macro = true;
                total_bytes += tok.text.len();
                out.push(tok.clone());
                i += 1;
                continue;
            }

            if self.has_unresolved_include && !def.certain_despite_unresolved {
                self.uncertain_macro_used = true;
            }
            self.had_expansion = true;

            let mut call_hide = tok
                .hide
                .union(&tokens[close_idx].hide)
                .cloned()
                .collect::<BTreeSet<_>>();
            call_hide.insert(tok.text.clone());
            let call_origin = if tok.origin.source_line == tokens[close_idx].origin.source_line {
                SourceRange {
                    source_line: tok.origin.source_line,
                    utf16_start: tok.origin.utf16_start,
                    utf16_end: tokens[close_idx].origin.utf16_end,
                }
            } else {
                tok.origin
            };

            let substituted = self.substitute_function_body(
                def,
                fixed_params,
                &raw_args,
                call_origin,
                &call_hide,
                depth + 1,
            );
            let rescanned = self.expand_seq(&substituted, depth + 1);
            for t in rescanned {
                total_bytes += t.text.len();
                out.push(t);
            }
            i = close_idx + 1;
        }

        out
    }

    fn substitute_function_body(
        &mut self,
        def: &MacroDef,
        fixed_params: &[String],
        raw_args: &[Vec<ExpToken>],
        call_origin: SourceRange,
        call_hide: &BTreeSet<String>,
        depth: usize,
    ) -> Vec<ExpToken> {
        let mut raw_map: HashMap<&str, Vec<ExpToken>> = HashMap::new();
        let mut exp_map: HashMap<&str, Vec<ExpToken>> = HashMap::new();

        for (idx, param) in fixed_params.iter().enumerate() {
            let trimmed = trim_edge_spaces(raw_args.get(idx).map(Vec::as_slice).unwrap_or(&[]));
            let expanded = self.expand_seq(&trimmed, depth);
            raw_map.insert(param.as_str(), trimmed);
            exp_map.insert(param.as_str(), expanded);
        }

        let mut va_empty = true;
        if let Some(va_name) = def.variadic_param.as_deref() {
            let mut joined = Vec::new();
            for (idx, arg) in raw_args.iter().skip(fixed_params.len()).enumerate() {
                if idx > 0 {
                    joined.push(ExpToken {
                        kind: TokenKind::Punct,
                        text: ",".to_owned(),
                        origin: call_origin,
                        hide: call_hide.clone(),
                    });
                    joined.push(ExpToken {
                        kind: TokenKind::Space,
                        text: " ".to_owned(),
                        origin: call_origin,
                        hide: call_hide.clone(),
                    });
                }
                let trimmed = trim_edge_spaces(arg);
                if !trimmed.is_empty() {
                    va_empty = false;
                }
                joined.extend(trimmed);
            }
            let expanded = self.expand_seq(&joined, depth);
            raw_map.insert(va_name, joined);
            exp_map.insert(va_name, expanded);
        }

        let body_toks = tokenize_with_origin(&def.body, call_origin, call_hide);
        let mut sub = Vec::with_capacity(body_toks.len());
        let mut bi = 0usize;

        while bi < body_toks.len() {
            let bt = &body_toks[bi];
            // `# param` stringification
            if bt.kind == TokenKind::Hash {
                let mut next = bi + 1;
                while next < body_toks.len() && body_toks[next].kind == TokenKind::Space {
                    next += 1;
                }
                if next < body_toks.len()
                    && body_toks[next].kind == TokenKind::Ident
                    && let Some(raw_param) = raw_map.get(body_toks[next].text.as_str())
                {
                    sub.push(ExpToken {
                        kind: TokenKind::StringLit,
                        text: stringify_tokens(raw_param),
                        origin: call_origin,
                        hide: call_hide.clone(),
                    });
                    bi = next + 1;
                    continue;
                }
            }

            // `##` token pasting (and GNU `, ##__VA_ARGS__`)
            if bt.kind == TokenKind::HashHash {
                let mut next = bi + 1;
                while next < body_toks.len() && body_toks[next].kind == TokenKind::Space {
                    next += 1;
                }
                // Drop whitespace immediately before `##`
                while sub.last().is_some_and(|t| t.kind == TokenKind::Space) {
                    sub.pop();
                }

                // GNU `, ##__VA_ARGS__`: drop preceding comma when variadic args are empty
                if next < body_toks.len()
                    && def.variadic_param.as_deref() == Some(body_toks[next].text.as_str())
                    && va_empty
                {
                    if sub.last().is_some_and(|t| t.text == ",") {
                        sub.pop();
                    }
                    bi = next + 1;
                    continue;
                }

                let lhs_tokens: Vec<ExpToken> = if let Some(prev) = sub.pop() {
                    vec![prev]
                } else {
                    Vec::new()
                };
                let rhs_tokens: Vec<ExpToken> = if next < body_toks.len() {
                    let rt = &body_toks[next];
                    if rt.kind == TokenKind::Ident
                        && let Some(raw_param) = raw_map.get(rt.text.as_str())
                    {
                        raw_param.clone()
                    } else {
                        vec![rt.clone()]
                    }
                } else {
                    Vec::new()
                };
                paste_tokens(&mut sub, &lhs_tokens, &rhs_tokens, call_origin, call_hide);
                bi = (next + 1).min(body_toks.len());
                continue;
            }

            // Before `##`, parameter takes its unexpanded tokens so `a ## b` pastes raw args.
            let mut look = bi + 1;
            while look < body_toks.len() && body_toks[look].kind == TokenKind::Space {
                look += 1;
            }
            let followed_by_hashhash =
                look < body_toks.len() && body_toks[look].kind == TokenKind::HashHash;

            if bt.kind == TokenKind::Ident
                && let Some(arg_toks) = if followed_by_hashhash {
                    raw_map.get(bt.text.as_str())
                } else {
                    exp_map.get(bt.text.as_str())
                }
            {
                for at in arg_toks {
                    let mut cloned = at.clone();
                    cloned.origin = call_origin;
                    cloned.hide.extend(call_hide.iter().cloned());
                    sub.push(cloned);
                }
                bi += 1;
                continue;
            }

            sub.push(bt.clone());
            bi += 1;
        }

        sub
    }
}

fn paste_tokens(
    out: &mut Vec<ExpToken>,
    lhs: &[ExpToken],
    rhs: &[ExpToken],
    origin: SourceRange,
    hide: &BTreeSet<String>,
) {
    match (lhs.split_last(), rhs.split_first()) {
        (Some((l_last, l_rest)), Some((r_first, r_rest))) => {
            out.extend_from_slice(l_rest);
            let combined = format!("{}{}", l_last.text, r_first.text);
            out.extend(tokenize_with_origin(&combined, origin, hide));
            out.extend_from_slice(r_rest);
        }
        (Some(_), None) => out.extend_from_slice(lhs),
        (None, Some(_)) => out.extend_from_slice(rhs),
        (None, None) => {}
    }
}

fn parse_invocation_args(
    tokens: &[ExpToken],
    open_paren_idx: usize,
) -> Option<(Vec<Vec<ExpToken>>, usize)> {
    let mut args = Vec::new();
    let mut current = Vec::new();
    let mut depth = 1usize;
    let mut i = open_paren_idx + 1;

    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.kind == TokenKind::Punct && tok.text == "(" {
            depth += 1;
            current.push(tok.clone());
        } else if tok.kind == TokenKind::Punct && tok.text == ")" {
            depth -= 1;
            if depth == 0 {
                args.push(current);
                return Some((args, i));
            }
            current.push(tok.clone());
        } else if tok.kind == TokenKind::Punct && tok.text == "," && depth == 1 {
            args.push(std::mem::take(&mut current));
        } else {
            current.push(tok.clone());
        }
        i += 1;
    }
    None
}

fn trim_edge_spaces(toks: &[ExpToken]) -> Vec<ExpToken> {
    let start = toks
        .iter()
        .position(|t| t.kind != TokenKind::Space)
        .unwrap_or(toks.len());
    let end = toks
        .iter()
        .rposition(|t| t.kind != TokenKind::Space)
        .map_or(start, |p| p + 1);
    toks[start..end].to_vec()
}

fn tokenize_with_origin(text: &str, origin: SourceRange, hide: &BTreeSet<String>) -> Vec<ExpToken> {
    tokenize(text)
        .into_iter()
        .map(|t| ExpToken {
            kind: t.kind,
            text: t.text,
            origin,
            hide: hide.clone(),
        })
        .collect()
}

fn stringify_tokens(toks: &[ExpToken]) -> String {
    let mut raw = String::new();
    let mut prev_space = false;
    for t in trim_edge_spaces(toks) {
        if t.kind == TokenKind::Space {
            if !prev_space {
                raw.push(' ');
                prev_space = true;
            }
        } else {
            raw.push_str(&t.text);
            prev_space = false;
        }
    }
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('"');
    for ch in raw.chars() {
        if ch == '\\' || ch == '"' {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}
