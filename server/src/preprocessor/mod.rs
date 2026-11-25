mod expand;
pub mod expr;
pub mod predefined;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::context::{FileContext, MacroFlag, path_key};
use crate::data::{Database, texts_data};

use expand::{CharOrigin, expand_logical_line};
use expr::{CondResult, eval_if_expr, eval_ifdef};
use predefined::predefined_macros;

const MAX_INCLUDE_DEPTH: usize = 32;
const MAX_INCLUDE_BYTES: u64 = 2 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineState {
    Active,
    Inactive,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedSpan {
    pub expanded: (usize, usize),
    pub source_line: usize,
    pub source_utf16: (u32, u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDef {
    pub params: Option<Vec<String>>,
    pub variadic_param: Option<String>,
    pub body: String,
    pub certain_despite_unresolved: bool,
}

#[derive(Debug, Clone)]
pub struct PreprocessedLine {
    pub source_line: usize,
    pub end_source_line: usize,
    pub state: LineState,
    pub text: String,
    pub had_expansion: bool,
    pub has_unexpanded_macro: bool,
    pub spans: Vec<ExpandedSpan>,
}

#[derive(Debug, Clone)]
pub struct PreprocessedFile {
    pub lines: Vec<PreprocessedLine>,
    pub macros: HashMap<String, MacroDef>,
    pub has_unresolved_include: bool,
}

impl PreprocessedFile {
    pub fn line_for_source(&self, source_line: usize) -> Option<&PreprocessedLine> {
        self.lines
            .iter()
            .find(|l| source_line >= l.source_line && source_line <= l.end_source_line)
    }
}

pub struct PreprocessOptions<'a> {
    pub file_path: Option<&'a Path>,
    pub file_ctx: &'a FileContext,
    pub db: &'a Database,
    pub open_files: Option<&'a HashMap<PathBuf, String>>,
}

struct LogicalLine {
    first_source_line: usize,
    last_source_line: usize,
    text: String,
    origins: Vec<CharOrigin>,
}

#[derive(Debug, Clone, Copy)]
struct CondFrame {
    parent_state: LineState,
    branch_state: LineState,
    any_true_taken: bool,
    saw_uncertain: bool,
    saw_else: bool,
}

/// Preprocess a document in pure Rust, evaluating conditionals, macros, and local includes.
pub fn preprocess_document(text: &str, opts: &PreprocessOptions<'_>) -> PreprocessedFile {
    preprocess_document_up_to(text, opts, None)
}

/// Preprocess a document only through `max_source_line` (inclusive) when hovering a line.
pub fn preprocess_document_up_to(
    text: &str,
    opts: &PreprocessOptions<'_>,
    max_source_line: Option<usize>,
) -> PreprocessedFile {
    if !opts.file_ctx.preprocessed {
        return passthrough_non_preprocessed(text, max_source_line);
    }

    let mut macros = seed_macro_table(opts);
    let mut has_unresolved_include = false;
    let mut active_includes = HashSet::new();
    if let Some(p) = opts.file_path {
        active_includes.insert(path_key(p));
    }

    // Apply CLI `-include` and `-imacros` before line 0 of the main file.
    for fi in &opts.file_ctx.force_includes {
        let _ = fi.kind;
        if let Some(inc_text) = read_include_candidate(&fi.path, opts.open_files) {
            process_include_text(
                &inc_text,
                &fi.path,
                opts,
                &mut macros,
                &mut has_unresolved_include,
                &mut active_includes,
                0,
            );
        } else {
            has_unresolved_include = true;
        }
    }

    let lines = process_source_lines(
        text,
        opts.file_path,
        opts,
        &mut macros,
        &mut has_unresolved_include,
        &mut active_includes,
        0,
        max_source_line,
        true,
    );

    PreprocessedFile {
        lines,
        macros,
        has_unresolved_include,
    }
}

fn passthrough_non_preprocessed(text: &str, max_source_line: Option<usize>) -> PreprocessedFile {
    let mut lines = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        if max_source_line.is_some_and(|limit| idx > limit) {
            break;
        }
        let line = raw.trim_end_matches('\r');
        let utf16_len = line.encode_utf16().count() as u32;
        let spans = if line.is_empty() {
            Vec::new()
        } else {
            vec![ExpandedSpan {
                expanded: (0, line.len()),
                source_line: idx,
                source_utf16: (0, utf16_len),
            }]
        };
        lines.push(PreprocessedLine {
            source_line: idx,
            end_source_line: idx,
            state: LineState::Active,
            text: line.to_owned(),
            had_expansion: false,
            has_unexpanded_macro: false,
            spans,
        });
    }
    PreprocessedFile {
        lines,
        macros: HashMap::new(),
        has_unresolved_include: false,
    }
}

fn seed_macro_table(opts: &PreprocessOptions<'_>) -> HashMap<String, MacroDef> {
    let mut macros = HashMap::new();
    for (name, val) in predefined_macros(
        &opts.file_ctx.march,
        opts.file_ctx.abi.as_deref(),
        opts.file_ctx.cmodel.as_deref(),
        opts.db,
    ) {
        macros.insert(
            name,
            MacroDef {
                params: None,
                variadic_param: None,
                body: val,
                certain_despite_unresolved: true,
            },
        );
    }

    for flag in &opts.file_ctx.macro_flags {
        match flag {
            MacroFlag::Define { name, value } => {
                macros.insert(
                    name.clone(),
                    MacroDef {
                        params: None,
                        variadic_param: None,
                        body: value.as_deref().unwrap_or("1").to_owned(),
                        certain_despite_unresolved: true,
                    },
                );
            }
            MacroFlag::Undef(name) => {
                macros.remove(name);
            }
        }
    }
    macros
}

#[allow(clippy::too_many_arguments)]
fn process_source_lines(
    text: &str,
    current_file: Option<&Path>,
    opts: &PreprocessOptions<'_>,
    macros: &mut HashMap<String, MacroDef>,
    has_unresolved_include: &mut bool,
    active_includes: &mut HashSet<PathBuf>,
    depth: usize,
    max_source_line: Option<usize>,
    collect_lines: bool,
) -> Vec<PreprocessedLine> {
    let logical_lines = collect_logical_lines(text);
    let mut stack: Vec<CondFrame> = Vec::new();
    let mut out = Vec::new();

    for lline in logical_lines {
        if max_source_line.is_some_and(|limit| lline.first_source_line > limit) {
            break;
        }
        let cur_state = stack
            .last()
            .map_or(LineState::Active, |frame| frame.branch_state);

        if let Some(kind) = classify_hash_line(&lline.text) {
            match kind {
                HashLineKind::Directive { name, rest, .. } => {
                    handle_directive(
                        name,
                        rest,
                        cur_state,
                        &mut stack,
                        current_file,
                        opts,
                        macros,
                        has_unresolved_include,
                        active_includes,
                        depth,
                    );
                }
                HashLineKind::LineMarker { .. }
                | HashLineKind::NullDirective { .. }
                | HashLineKind::Comment { .. } => {}
            }
            if collect_lines {
                out.push(PreprocessedLine {
                    source_line: lline.first_source_line,
                    end_source_line: lline.last_source_line,
                    state: cur_state,
                    text: String::new(),
                    had_expansion: false,
                    has_unexpanded_macro: false,
                    spans: Vec::new(),
                });
            }
            continue;
        }

        if !collect_lines {
            continue;
        }

        if cur_state == LineState::Inactive {
            out.push(PreprocessedLine {
                source_line: lline.first_source_line,
                end_source_line: lline.last_source_line,
                state: LineState::Inactive,
                text: String::new(),
                had_expansion: false,
                has_unexpanded_macro: false,
                spans: Vec::new(),
            });
            continue;
        }

        let code_end = find_unquoted_hash_comment(&lline.text).unwrap_or(lline.text.len());
        let code_slice = &lline.text[..code_end];
        let trimmed_code = code_slice.trim();
        if trimmed_code.is_empty() {
            out.push(PreprocessedLine {
                source_line: lline.first_source_line,
                end_source_line: lline.last_source_line,
                state: cur_state,
                text: String::new(),
                had_expansion: false,
                has_unexpanded_macro: false,
                spans: Vec::new(),
            });
            continue;
        }

        let expanded =
            expand_logical_line(code_slice, &lline.origins, macros, *has_unresolved_include);
        out.push(PreprocessedLine {
            source_line: lline.first_source_line,
            end_source_line: lline.last_source_line,
            state: cur_state,
            text: expanded.text,
            had_expansion: expanded.had_expansion,
            has_unexpanded_macro: expanded.has_unexpanded_macro,
            spans: expanded.spans,
        });
    }

    out
}

#[allow(clippy::too_many_arguments)]
fn handle_directive(
    name: &str,
    rest: &str,
    cur_state: LineState,
    stack: &mut Vec<CondFrame>,
    current_file: Option<&Path>,
    opts: &PreprocessOptions<'_>,
    macros: &mut HashMap<String, MacroDef>,
    has_unresolved_include: &mut bool,
    active_includes: &mut HashSet<PathBuf>,
    depth: usize,
) {
    match name {
        "if" | "ifdef" | "ifndef" => {
            if cur_state == LineState::Inactive {
                stack.push(CondFrame {
                    parent_state: LineState::Inactive,
                    branch_state: LineState::Inactive,
                    any_true_taken: true,
                    saw_uncertain: false,
                    saw_else: false,
                });
                return;
            }
            let cond = match name {
                "if" => eval_if_expr(rest, macros, *has_unresolved_include),
                "ifdef" => eval_ifdef_rest(rest, false, macros, *has_unresolved_include),
                "ifndef" => eval_ifdef_rest(rest, true, macros, *has_unresolved_include),
                _ => CondResult::Uncertain,
            };
            push_cond_frame(stack, cur_state, cond);
        }
        "elif" | "elifdef" | "elifndef" => {
            let Some(frame) = stack.last_mut() else {
                return;
            };
            if frame.parent_state == LineState::Inactive || frame.saw_else {
                frame.branch_state = LineState::Inactive;
                return;
            }
            if frame.any_true_taken {
                frame.branch_state = LineState::Inactive;
                return;
            }
            let cond = match name {
                "elif" => eval_if_expr(rest, macros, *has_unresolved_include),
                "elifdef" => eval_ifdef_rest(rest, false, macros, *has_unresolved_include),
                "elifndef" => eval_ifdef_rest(rest, true, macros, *has_unresolved_include),
                _ => CondResult::Uncertain,
            };
            let next_state = resolve_branch_state(frame.parent_state, cond);
            if matches!(cond, CondResult::True) {
                frame.any_true_taken = true;
            } else if matches!(cond, CondResult::Uncertain) {
                frame.saw_uncertain = true;
            }
            frame.branch_state = if frame.saw_uncertain && next_state != LineState::Inactive {
                LineState::Uncertain
            } else {
                next_state
            };
        }
        "else" => {
            let Some(frame) = stack.last_mut() else {
                return;
            };
            if frame.parent_state == LineState::Inactive || frame.saw_else || frame.any_true_taken {
                frame.branch_state = LineState::Inactive;
            } else if frame.saw_uncertain || frame.parent_state == LineState::Uncertain {
                frame.branch_state = LineState::Uncertain;
            } else {
                frame.branch_state = LineState::Active;
                frame.any_true_taken = true;
            }
            frame.saw_else = true;
        }
        "endif" => {
            stack.pop();
        }
        "define" if cur_state != LineState::Inactive => {
            if let Some((macro_name, def)) = parse_macro_def(rest) {
                macros.insert(macro_name, def);
            }
        }
        "undef" if cur_state != LineState::Inactive => {
            if let Some(ident) = first_ident(rest) {
                macros.remove(ident);
            }
        }
        "include" | "include_next" | "import" if cur_state != LineState::Inactive => {
            if let Some((target, is_system)) = parse_include_arg(rest) {
                if depth >= MAX_INCLUDE_DEPTH {
                    *has_unresolved_include = true;
                    return;
                }
                if let Some((resolved, content)) =
                    resolve_and_read_include(&target, is_system, current_file, opts)
                {
                    let key = path_key(&resolved);
                    if active_includes.insert(key.clone()) {
                        process_source_lines(
                            &content,
                            Some(&resolved),
                            opts,
                            macros,
                            has_unresolved_include,
                            active_includes,
                            depth + 1,
                            None,
                            false,
                        );
                        active_includes.remove(&key);
                    }
                } else {
                    *has_unresolved_include = true;
                }
            }
        }
        _ => {}
    }
}

fn push_cond_frame(stack: &mut Vec<CondFrame>, parent_state: LineState, cond: CondResult) {
    let branch_state = resolve_branch_state(parent_state, cond);
    stack.push(CondFrame {
        parent_state,
        branch_state,
        any_true_taken: matches!(cond, CondResult::True),
        saw_uncertain: matches!(cond, CondResult::Uncertain),
        saw_else: false,
    });
}

fn resolve_branch_state(parent_state: LineState, cond: CondResult) -> LineState {
    match (parent_state, cond) {
        (LineState::Inactive, _) | (_, CondResult::False) => LineState::Inactive,
        (LineState::Uncertain, _) | (_, CondResult::Uncertain) => LineState::Uncertain,
        (LineState::Active, CondResult::True) => LineState::Active,
    }
}

fn eval_ifdef_rest(
    rest: &str,
    negate: bool,
    macros: &HashMap<String, MacroDef>,
    has_unresolved_include: bool,
) -> CondResult {
    match first_ident(rest) {
        Some(ident) => eval_ifdef(ident, negate, macros, has_unresolved_include),
        None => CondResult::Uncertain,
    }
}

fn first_ident(s: &str) -> Option<&str> {
    let trimmed = s.trim_start_matches([' ', '\t']);
    let bytes = trimmed.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = 1usize;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    Some(&trimmed[..end])
}

fn parse_macro_def(rest: &str) -> Option<(String, MacroDef)> {
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let after_ws = rest.trim_start_matches([' ', '\t']);
    let ident = first_ident(after_ws)?;
    let after_ident = &after_ws[ident.len()..];

    if let Some(paren_rest) = after_ident.strip_prefix('(') {
        let close = paren_rest.find(')')?;
        let param_list = &paren_rest[..close];
        let body = paren_rest[close + 1..].trim().to_owned();
        let mut params = Vec::new();
        let mut variadic_param = None;

        for raw in param_list.split(',') {
            let p = raw.trim();
            if p.is_empty() {
                continue;
            }
            if p == "..." {
                variadic_param = Some("__VA_ARGS__".to_owned());
            } else if let Some(named_va) = p.strip_suffix("...") {
                let name = named_va.trim();
                if !name.is_empty() {
                    variadic_param = Some(name.to_owned());
                }
            } else if let Some(param_name) = first_ident(p) {
                params.push(param_name.to_owned());
            }
        }

        return Some((
            ident.to_owned(),
            MacroDef {
                params: Some(params),
                variadic_param,
                body,
                certain_despite_unresolved: true,
            },
        ));
    }

    if !(after_ident.is_empty() || after_ident.starts_with([' ', '\t'])) {
        return None;
    }

    Some((
        ident.to_owned(),
        MacroDef {
            params: None,
            variadic_param: None,
            body: after_ident.trim().to_owned(),
            certain_despite_unresolved: true,
        },
    ))
}

fn parse_include_arg(rest: &str) -> Option<(String, bool)> {
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

fn resolve_and_read_include(
    target: &str,
    is_system: bool,
    current_file: Option<&Path>,
    opts: &PreprocessOptions<'_>,
) -> Option<(PathBuf, String)> {
    let mut dirs: Vec<&Path> = Vec::new();
    if !is_system {
        if let Some(dir) = current_file.and_then(Path::parent) {
            dirs.push(dir);
        }
        for d in &opts.file_ctx.quote_dirs {
            dirs.push(d.as_path());
        }
    }
    for d in &opts.file_ctx.include_dirs {
        dirs.push(d.as_path());
    }
    if is_system && let Some(dir) = current_file.and_then(Path::parent) {
        dirs.push(dir);
    }
    if let Some(wd) = opts.file_ctx.working_dir.as_deref() {
        dirs.push(wd);
    }

    let rel = Path::new(target);
    if rel.is_absolute()
        && let Some(content) = read_include_candidate(rel, opts.open_files)
    {
        return Some((rel.to_path_buf(), content));
    }

    for dir in dirs {
        let cand = dir.join(rel);
        if let Some(content) = read_include_candidate(&cand, opts.open_files) {
            return Some((cand, content));
        }
    }
    None
}

fn read_include_candidate(
    path: &Path,
    open_files: Option<&HashMap<PathBuf, String>>,
) -> Option<String> {
    if let Some(open) = open_files {
        let key = path_key(path);
        if let Some(text) = open.get(&key).or_else(|| open.get(path)) {
            return Some(text.clone());
        }
    }
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INCLUDE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn process_include_text(
    text: &str,
    path: &Path,
    opts: &PreprocessOptions<'_>,
    macros: &mut HashMap<String, MacroDef>,
    has_unresolved_include: &mut bool,
    active_includes: &mut HashSet<PathBuf>,
    depth: usize,
) {
    let key = path_key(path);
    if !active_includes.insert(key.clone()) {
        return;
    }
    process_source_lines(
        text,
        Some(path),
        opts,
        macros,
        has_unresolved_include,
        active_includes,
        depth,
        None,
        false,
    );
    active_includes.remove(&key);
}

fn find_unquoted_hash_comment(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' && i + 1 < bytes.len() {
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
        } else if b == b'#' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Join backslash line continuations and strip C block/line comments while preserving
/// exact UTF-16 source coordinates for every character in each logical line.
fn collect_logical_lines(text: &str) -> Vec<LogicalLine> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut idx = 0usize;

    let mut phys_line = 0usize;
    let mut utf16_col = 0u32;

    let mut cur_first_line = 0usize;
    let mut cur_last_line = 0usize;
    let mut cur_text = String::new();
    let mut cur_origins: Vec<CharOrigin> = Vec::new();

    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let advance_pos = |ch: char, line: &mut usize, col: &mut u32| {
        if ch == '\n' {
            *line += 1;
            *col = 0;
        } else {
            *col += ch.len_utf16() as u32;
        }
    };

    while idx < chars.len() {
        let ch = chars[idx];
        // Line splicing: `\` followed by `\n` or `\r\n`.
        if ch == '\\' {
            if chars.get(idx + 1) == Some(&'\n') {
                phys_line += 1;
                utf16_col = 0;
                cur_last_line = phys_line;
                idx += 2;
                continue;
            }
            if chars.get(idx + 1) == Some(&'\r') && chars.get(idx + 2) == Some(&'\n') {
                phys_line += 1;
                utf16_col = 0;
                cur_last_line = phys_line;
                idx += 3;
                continue;
            }
        }

        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
                out.push(LogicalLine {
                    first_source_line: cur_first_line,
                    last_source_line: cur_last_line,
                    text: std::mem::take(&mut cur_text),
                    origins: std::mem::take(&mut cur_origins),
                });
                phys_line += 1;
                utf16_col = 0;
                cur_first_line = phys_line;
                cur_last_line = phys_line;
            } else if ch != '\r' {
                advance_pos(ch, &mut phys_line, &mut utf16_col);
            } else {
                utf16_col += 1;
            }
            idx += 1;
            continue;
        }

        if in_block_comment {
            if ch == '*' && chars.get(idx + 1) == Some(&'/') {
                let u_start = utf16_col;
                utf16_col += 2;
                cur_last_line = phys_line;
                let byte_offset = cur_text.len();
                cur_text.push(' ');
                cur_origins.push(CharOrigin {
                    byte_offset,
                    source_line: phys_line,
                    utf16_start: u_start,
                    utf16_end: utf16_col,
                });
                in_block_comment = false;
                idx += 2;
                continue;
            }
            if ch == '\r' {
                utf16_col += 1;
            } else {
                advance_pos(ch, &mut phys_line, &mut utf16_col);
            }
            idx += 1;
            continue;
        }

        if ch == '\r' && chars.get(idx + 1) == Some(&'\n') {
            utf16_col += 1;
            idx += 1;
            continue;
        }

        if ch == '\n' {
            in_single = false;
            in_double = false;
            out.push(LogicalLine {
                first_source_line: cur_first_line,
                last_source_line: cur_last_line,
                text: std::mem::take(&mut cur_text),
                origins: std::mem::take(&mut cur_origins),
            });
            phys_line += 1;
            utf16_col = 0;
            cur_first_line = phys_line;
            cur_last_line = phys_line;
            idx += 1;
            continue;
        }

        if in_single || in_double {
            let quote = if in_single { '\'' } else { '"' };
            let u_start = utf16_col;
            let u_len = ch.len_utf16() as u32;
            utf16_col += u_len;
            cur_last_line = phys_line;
            let byte_offset = cur_text.len();
            cur_text.push(ch);
            cur_origins.push(CharOrigin {
                byte_offset,
                source_line: phys_line,
                utf16_start: u_start,
                utf16_end: utf16_col,
            });
            if ch == '\\' && idx + 1 < chars.len() && chars[idx + 1] != '\n' {
                let next = chars[idx + 1];
                let nu_start = utf16_col;
                utf16_col += next.len_utf16() as u32;
                let nb_offset = cur_text.len();
                cur_text.push(next);
                cur_origins.push(CharOrigin {
                    byte_offset: nb_offset,
                    source_line: phys_line,
                    utf16_start: nu_start,
                    utf16_end: utf16_col,
                });
                idx += 2;
                continue;
            }
            if ch == quote {
                in_single = false;
                in_double = false;
            }
            idx += 1;
            continue;
        }

        if ch == '/' && chars.get(idx + 1) == Some(&'/') {
            in_line_comment = true;
            utf16_col += 2;
            idx += 2;
            continue;
        }
        if ch == '/' && chars.get(idx + 1) == Some(&'*') {
            in_block_comment = true;
            utf16_col += 2;
            idx += 2;
            continue;
        }
        if ch == '\'' {
            in_single = true;
        } else if ch == '"' {
            in_double = true;
        }

        let u_start = utf16_col;
        utf16_col += ch.len_utf16() as u32;
        cur_last_line = phys_line;
        let byte_offset = cur_text.len();
        cur_text.push(ch);
        cur_origins.push(CharOrigin {
            byte_offset,
            source_line: phys_line,
            utf16_start: u_start,
            utf16_end: utf16_col,
        });
        idx += 1;
    }

    if !cur_text.is_empty() || !text.ends_with('\n') && !text.is_empty() {
        out.push(LogicalLine {
            first_source_line: cur_first_line,
            last_source_line: cur_last_line,
            text: cur_text,
            origins: cur_origins,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{CompileContext, ForceInclude, ForceIncludeKind, MacroFlag};

    #[test]
    fn classifies_preprocessor_lines_matching_clang() {
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

    #[test]
    fn preprocesses_nested_conditionals_macros_continuations_and_multibyte_spans() {
        let db = Database::load().unwrap();
        let mut ctx = CompileContext::new(None, Config::default());
        let uri: lsp_types::Uri = "file:///tmp/test_b3.S".parse().unwrap();
        let file_ctx = ctx.for_uri(&uri);

        let src = concat!(
            "#define REG_PREFIX(n) a ## n\n",
            "#define ADD_PAIR(rd, rs1, imm) \\\n",
            "    addi REG_PREFIX(rd), /* inline */ rs1, imm\n",
            "#define STR(x) #x\n",
            "#if __riscv_xlen == 64\n",
            "#ifdef NOT_DEF\n",
            "    ebreak\n",
            "#elif 1 + 1 == 2\n",
            "    // 日本語コメント\n",
            "    /* 日 */ ADD_PAIR(0, a1, 7) // 😀\n",
            "#else\n",
            "    unimp\n",
            "#endif\n",
            "#endif\n",
            ".asciz STR(hello world)\n",
        );

        let pre = preprocess_document(
            src,
            &PreprocessOptions {
                file_path: None,
                file_ctx: &file_ctx,
                db: &db,
                open_files: None,
            },
        );
        assert!(!pre.has_unresolved_include);

        let ebreak = pre.line_for_source(6).unwrap();
        assert_eq!(ebreak.state, LineState::Inactive);

        let expanded = pre.line_for_source(9).unwrap();
        assert_eq!(expanded.state, LineState::Active);
        assert!(expanded.had_expansion);
        assert!(!expanded.has_unexpanded_macro);
        assert_eq!(expanded.text.trim(), "addi a0,   a1, 7");
        // Verify UTF-16 span points inside line 9 after `    /* 日 */ `
        assert!(
            expanded
                .spans
                .iter()
                .any(|s| s.source_line == 9 && s.source_utf16.0 >= 12)
        );

        let unimp = pre.line_for_source(11).unwrap();
        assert_eq!(unimp.state, LineState::Inactive);

        let asciz = pre.line_for_source(14).unwrap();
        assert_eq!(asciz.text.trim(), r#".asciz "hello world""#);
    }

    #[test]
    fn handles_variadic_recursive_macros_cli_flags_and_uncertain_includes() {
        let db = Database::load().unwrap();
        let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_b3_inc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let hdr = tmp.join("defs.h");
        let forced = tmp.join("forced.h");
        fs::write(&hdr, "#define HDR_IMM 42\n").unwrap();
        fs::write(&forced, "#define FORCED_REG s0\n").unwrap();

        let main_path = tmp.join("main.S");
        let mut ctx = CompileContext::new(Some(tmp.clone()), Config::default());
        let mut file_ctx = ctx.for_uri(&crate::context::path_to_uri(&main_path).unwrap());
        file_ctx.macro_flags = vec![
            MacroFlag::Define {
                name: "CLI_VAL".to_owned(),
                value: Some("99".to_owned()),
            },
            MacroFlag::Define {
                name: "GONE".to_owned(),
                value: None,
            },
            MacroFlag::Undef("GONE".to_owned()),
        ];
        file_ctx.force_includes = vec![ForceInclude {
            kind: ForceIncludeKind::Include,
            path: forced,
        }];

        let src = "\
#include \"defs.h\"
#define SELF SELF
#define CALL(fn, ...) fn(a0, ##__VA_ARGS__)
    addi FORCED_REG, zero, HDR_IMM
    li a1, CLI_VAL
#ifdef GONE
    ecall
#endif
    CALL(foo)
    CALL(bar, a1, a2)
    mv t0, SELF
#include \"missing_header.h\"
#ifdef UNKNOWN_SYM
    ebreak
#endif
#if __riscv_xlen == 64
    nop
#endif
";
        let pre = preprocess_document(
            src,
            &PreprocessOptions {
                file_path: Some(&main_path),
                file_ctx: &file_ctx,
                db: &db,
                open_files: None,
            },
        );
        assert!(pre.has_unresolved_include);
        assert_eq!(
            pre.line_for_source(3).unwrap().text.trim(),
            "addi s0, zero, 42"
        );
        assert_eq!(pre.line_for_source(4).unwrap().text.trim(), "li a1, 99");
        assert_eq!(pre.line_for_source(6).unwrap().state, LineState::Inactive);
        assert_eq!(pre.line_for_source(8).unwrap().text.trim(), "foo(a0)");
        assert_eq!(
            pre.line_for_source(9).unwrap().text.trim(),
            "bar(a0,a1, a2)"
        );
        assert_eq!(pre.line_for_source(10).unwrap().text.trim(), "mv t0, SELF");
        assert_eq!(pre.line_for_source(13).unwrap().state, LineState::Uncertain);
        assert_eq!(pre.line_for_source(16).unwrap().state, LineState::Active);

        let crlf = "#define CRLF_INSN(r) \\\r\n    mv r, a0\r\n    CRLF_INSN(a1)\r\n";
        let pre_crlf = preprocess_document(
            crlf,
            &PreprocessOptions {
                file_path: None,
                file_ctx: &file_ctx,
                db: &db,
                open_files: None,
            },
        );
        assert_eq!(
            pre_crlf.line_for_source(2).unwrap().text.trim(),
            "mv a1, a0"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
