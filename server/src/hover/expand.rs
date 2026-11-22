use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::tools::{ToolKind, ToolRunner};

pub(super) const MAX_EXPANSION_CACHE_ENTRIES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ExpansionCacheKey {
    pub(super) stmt: String,
    pub(super) triple: String,
    pub(super) mattr: String,
    pub(super) option_lines: Vec<String>,
    pub(super) const_pairs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExpandedInstruction {
    pub(super) asm: String,
    pub(super) encoding: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum CachedExpansion {
    Expanded(Vec<ExpandedInstruction>),
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct PendingExpansion {
    pub(super) key: ExpansionCacheKey,
    pub(super) clang_flags: Vec<String>,
    pub(super) file_dir: Option<PathBuf>,
    pub(super) working_dir: Option<PathBuf>,
    pub(super) cpp_preamble: Vec<String>,
    pub(super) known_consts: HashMap<String, String>,
}

pub(super) fn append_cached_expansion(mut md: String, cached: &CachedExpansion) -> String {
    match cached {
        CachedExpansion::Expanded(items) if items.len() == 1 => {
            let item = &items[0];
            match item.encoding.as_deref() {
                Some("(relocated)") => {
                    let _ = write!(md, "\n\n**This line:** `{}` · (relocated)", item.asm);
                }
                Some(hex) => {
                    let _ = write!(md, "\n\n**This line:** `{}` · `{hex}`", item.asm);
                }
                None => {
                    let _ = write!(md, "\n\n**This line:** `{}`", item.asm);
                }
            }
        }
        CachedExpansion::Expanded(items) => {
            let _ = writeln!(md, "\n\n**This line:**\n```riscv");
            for item in items {
                if let Some(enc) = &item.encoding {
                    let _ = writeln!(md, "{}  # {enc}", item.asm);
                } else {
                    let _ = writeln!(md, "{}", item.asm);
                }
            }
            md.push_str("```");
        }
        CachedExpansion::Unavailable => {
            md.push_str("\n\n**This line:** *expansion unavailable*");
        }
    }
    md
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

    // 3. Preprocess with `clang -E -P -x assembler-with-cpp` when preprocessor directives exist
    if !pending.cpp_preamble.is_empty()
        && !tools.is_disabled(ToolKind::Clang)
        && let Some(pre) = preprocess_with_clang(
            &key.stmt,
            &pending.cpp_preamble,
            &pending.clang_flags,
            pending.file_dir.as_deref(),
            pending.working_dir.as_deref(),
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

pub(super) fn collect_cpp_preamble(text: &str, up_to_line: usize) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if idx >= up_to_line {
            break;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let kw = rest.trim_start();
            if kw.starts_with("include")
                || kw.starts_with("define")
                || kw.starts_with("undef")
                || kw.starts_with("if")
                || kw.starts_with("else")
                || kw.starts_with("elif")
                || kw.starts_with("endif")
            {
                out.push(trimmed.to_owned());
            }
        }
    }
    out
}

fn substitute_constants(stmt: &str, consts: &HashMap<String, String>) -> String {
    let mut current = stmt.to_owned();
    for _ in 0..3 {
        let mut changed = false;
        let mut keys: Vec<&String> = consts.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        for name in keys {
            if contains_ident(&current, name)
                && let Some(val) = consts.get(name)
            {
                let replaced = replace_whole_ident(&current, name, &format!("({val})"));
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
