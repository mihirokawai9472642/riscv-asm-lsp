use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::tools::{ToolKind, ToolRunner};

pub(super) const MAX_EXPANSION_CACHE_ENTRIES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ExpansionCacheKey {
    pub(super) stmt: String,
    pub(super) builtin_preprocessed_stmt: Option<String>,
    pub(super) builtin_had_unresolved_include: bool,
    pub(super) triple: String,
    pub(super) mattr: String,
    pub(super) option_lines: Vec<String>,
    pub(super) const_pairs: Vec<(String, String)>,
    pub(super) clang_flags: Vec<String>,
    pub(super) cpp_preamble: Vec<String>,
    pub(super) open_if_depth: usize,
    pub(super) file_dir: Option<PathBuf>,
    pub(super) working_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedInstruction {
    pub asm: String,
    pub encoding: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum CachedExpansion {
    Expanded(Vec<ExpandedInstruction>),
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct PendingExpansion {
    pub(super) key: ExpansionCacheKey,
    pub(super) known_consts: HashMap<String, String>,
}

pub(super) fn format_this_line_fact(cached: &CachedExpansion) -> String {
    match cached {
        CachedExpansion::Expanded(items) if items.len() == 1 => {
            let item = &items[0];
            match item.encoding.as_deref() {
                Some("(relocated)") => format!("This line: `{}` (relocated)", item.asm),
                Some(hex) => format!("This line: `{}` → `{hex}`", item.asm),
                None => format!("This line: `{}`", item.asm),
            }
        }
        CachedExpansion::Expanded(items) => {
            let mut out = String::from("This line:\n```riscv\n");
            for item in items {
                if let Some(enc) = &item.encoding {
                    let _ = writeln!(out, "{}  # {enc}", item.asm);
                } else {
                    let _ = writeln!(out, "{}", item.asm);
                }
            }
            out.push_str("```");
            out
        }
        CachedExpansion::Unavailable => "This line: *expansion unavailable*".to_owned(),
    }
}

pub(super) fn resolve_expansion(pending: &PendingExpansion, tools: &ToolRunner) -> CachedExpansion {
    let key = &pending.key;

    // 1. Try expanding the statement directly
    if let Some(exp) =
        run_llvm_mc_expand(&key.stmt, &key.triple, &key.mattr, &key.option_lines, tools)
    {
        return CachedExpansion::Expanded(exp);
    }

    // 2. Substitute known `#define` / `.equ` constant values in scope
    if !pending.known_consts.is_empty() {
        let substituted = substitute_constants(&key.stmt, &pending.known_consts);
        if substituted != key.stmt
            && let Some(exp) = run_llvm_mc_expand(
                &substituted,
                &key.triple,
                &key.mattr,
                &key.option_lines,
                tools,
            )
        {
            return CachedExpansion::Expanded(exp);
        }
    }

    // 3. Try the built-in pure-Rust C preprocessor result before spawning external `clang -E`
    if let Some(pre) = key.builtin_preprocessed_stmt.as_deref() {
        let resolved = substitute_constants(pre, &pending.known_consts);
        if !resolved.is_empty()
            && resolved != key.stmt
            && let Some(exp) =
                run_llvm_mc_expand(&resolved, &key.triple, &key.mattr, &key.option_lines, tools)
        {
            return CachedExpansion::Expanded(exp);
        }
    }

    // 4. Fallback to `clang -E -P -x assembler-with-cpp` when external tool is available
    if (!key.cpp_preamble.is_empty() || key.builtin_had_unresolved_include)
        && !tools.is_disabled(ToolKind::Clang)
        && let Some(pre) = preprocess_with_clang(
            &key.stmt,
            &key.cpp_preamble,
            key.open_if_depth,
            &key.clang_flags,
            key.file_dir.as_deref(),
            key.working_dir.as_deref(),
            tools,
        )
    {
        let resolved = substitute_constants(&pre, &pending.known_consts);
        if let Some(exp) =
            run_llvm_mc_expand(&resolved, &key.triple, &key.mattr, &key.option_lines, tools)
        {
            return CachedExpansion::Expanded(exp);
        }
    }

    CachedExpansion::Unavailable
}

fn run_llvm_mc_expand(
    stmt: &str,
    triple: &str,
    mattr: &str,
    option_lines: &[String],
    tools: &ToolRunner,
) -> Option<Vec<ExpandedInstruction>> {
    let mut args = vec![format!("-triple={triple}")];
    if !mattr.is_empty() {
        args.push(format!("-mattr={mattr}"));
    }
    args.push("-show-encoding".to_owned());
    args.push("-M".to_owned());
    args.push("no-aliases".to_owned());

    let mut stdin_buf = String::new();
    for opt in option_lines {
        stdin_buf.push_str(opt);
        stdin_buf.push('\n');
    }
    stdin_buf.push_str(stmt);
    stdin_buf.push('\n');

    let output = tools.run_llvm_mc(&args, &stdin_buf)?;
    if !output.success {
        return None;
    }

    parse_llvm_mc_output(&output.stdout)
}

pub(super) fn parse_llvm_mc_output(stdout: &str) -> Option<Vec<ExpandedInstruction>> {
    let mut items = Vec::new();
    for raw in stdout.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('.')
            || trimmed.starts_with('#')
            || trimmed.ends_with(':')
        {
            continue;
        }
        let (asm_raw, enc_opt) = if let Some((left, right)) = trimmed.split_once("# encoding:") {
            (left, format_encoding_bytes(right))
        } else {
            (trimmed, None)
        };
        let asm = asm_raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if !asm.is_empty() {
            items.push(ExpandedInstruction {
                asm,
                encoding: enc_opt,
            });
        }
    }
    if items.is_empty() { None } else { Some(items) }
}

fn format_encoding_bytes(enc_part: &str) -> Option<String> {
    let open = enc_part.find('[')?;
    let close = enc_part[open + 1..].find(']')?;
    let inner = enc_part[open + 1..open + 1 + close].trim();
    if inner.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = inner.split(',').map(str::trim).collect();
    let mut bytes = Vec::with_capacity(tokens.len());
    for tok in &tokens {
        let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) else {
            return Some("(relocated)".to_owned());
        };
        if hex.is_empty() || hex.len() > 2 {
            return Some("(relocated)".to_owned());
        }
        let Ok(b) = u8::from_str_radix(hex, 16) else {
            return Some("(relocated)".to_owned());
        };
        bytes.push(b);
    }

    match bytes.len() {
        2 => Some(format!(
            "0x{:04x}",
            u16::from_le_bytes([bytes[0], bytes[1]])
        )),
        n if n >= 4 && n % 4 == 0 => {
            let words: Vec<String> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| format!("0x{:08x}", u32::from_le_bytes(*c)))
                .collect();
            Some(words.join(" "))
        }
        _ => {
            let raw: Vec<String> = bytes.iter().map(|b| format!("0x{b:02x}")).collect();
            Some(raw.join(" "))
        }
    }
}

fn preprocess_with_clang(
    stmt: &str,
    cpp_preamble: &[String],
    open_if_depth: usize,
    clang_flags: &[String],
    file_dir: Option<&Path>,
    working_dir: Option<&Path>,
    tools: &ToolRunner,
) -> Option<String> {
    let mut args = clang_flags.to_vec();
    if let Some(dir) = file_dir {
        args.push("-iquote".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.extend([
        "-E".to_owned(),
        "-P".to_owned(),
        "-x".to_owned(),
        "assembler-with-cpp".to_owned(),
        "-".to_owned(),
    ]);

    let mut stdin_buf = cpp_preamble.join("\n");
    if !stdin_buf.is_empty() {
        stdin_buf.push('\n');
    }
    stdin_buf.push_str(stmt);
    stdin_buf.push('\n');
    for _ in 0..open_if_depth {
        stdin_buf.push_str("#endif\n");
    }

    let out = tools.run_clang(&args, &stdin_buf, working_dir)?;
    if !out.success {
        return None;
    }
    let last_stmt = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if last_stmt.is_empty() {
        None
    } else {
        Some(last_stmt)
    }
}

pub(super) fn collect_cpp_preamble(text: &str, up_to_line: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut in_continuation = false;
    let mut if_depth = 0usize;

    for (idx, line) in text.lines().enumerate() {
        if idx >= up_to_line {
            break;
        }
        let started_in_block = in_block;
        let _ = crate::line::strip_comments_with_state(line, &mut in_block, true);

        if in_continuation {
            let clean = line.trim_end_matches('\r');
            out.push(clean.to_owned());
            in_continuation = clean.ends_with('\\');
            continue;
        }
        if started_in_block {
            continue;
        }
        if let Some(crate::preprocessor::HashLineKind::Directive { name, .. }) =
            crate::preprocessor::classify_hash_line(line)
            && crate::preprocessor::is_cpp_preamble_line(line)
        {
            match name {
                "if" | "ifdef" | "ifndef" => if_depth += 1,
                "endif" => if_depth = if_depth.saturating_sub(1),
                _ => {}
            }
            let clean = line.trim().trim_end_matches('\r');
            out.push(clean.to_owned());
            in_continuation = clean.ends_with('\\');
        }
    }
    (out, if_depth)
}

pub(super) fn substitute_constants(stmt: &str, consts: &HashMap<String, String>) -> String {
    let mut current = stmt.to_owned();
    for _ in 0..3 {
        let mut changed = false;
        let mut keys: Vec<&String> = consts.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        for name in keys {
            if contains_ident(&current, name)
                && let Some(val) = consts.get(name)
            {
                let trimmed = val.trim();
                let is_bare_token = !trimmed.is_empty()
                    && trimmed
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$'));
                let rep = if is_bare_token {
                    trimmed.to_owned()
                } else {
                    format!("({trimmed})")
                };
                let replaced = replace_whole_ident(&current, name, &rep);
                if replaced != current {
                    current = replaced;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    current
}

pub(super) fn contains_ident(haystack: &str, ident: &str) -> bool {
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
            return true;
        }
        i = start + 1;
    }
    false
}

fn replace_whole_ident(haystack: &str, ident: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let bytes = haystack.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if haystack[i..].starts_with(ident) {
            let before_ok =
                i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
            let end = i + ident.len();
            let after_ok =
                end >= bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
            if before_ok && after_ok {
                out.push_str(replacement);
                i = end;
                continue;
            }
        }
        if let Some(ch) = haystack[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}
