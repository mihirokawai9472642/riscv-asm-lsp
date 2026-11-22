use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use lsp_server::{Message, RequestId, Response};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};

use crate::context::{CompileContext, FileContext, effective_options_at_line, uri_to_path};
use crate::data::{
    CsrEntry, Database, DirectiveEntry, InstructionEntry, InstructionKind, RegisterEntry, Xlen,
};
use crate::diagnostics::MessageSink;
use crate::document::Document;
use crate::index::{SymKind, SymbolHoverInfo, WorkspaceIndex};
use crate::position::word_range_at;
use crate::tools::{ToolKind, ToolRunner};

const MAX_EXPANSION_CACHE_ENTRIES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExpansionCacheKey {
    stmt: String,
    triple: String,
    mattr: String,
    option_lines: Vec<String>,
    const_pairs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedInstruction {
    asm: String,
    encoding: Option<String>,
}

#[derive(Debug, Clone)]
enum CachedExpansion {
    Expanded(Vec<ExpandedInstruction>),
    Unavailable,
}

#[derive(Clone, Default)]
pub struct ExpansionCache(Arc<Mutex<HashMap<ExpansionCacheKey, CachedExpansion>>>);

pub fn new_expansion_cache() -> ExpansionCache {
    ExpansionCache::default()
}

#[derive(Debug, Clone)]
pub struct PendingExpansion {
    key: ExpansionCacheKey,
    clang_flags: Vec<String>,
    file_dir: Option<PathBuf>,
    cpp_preamble: Vec<String>,
    known_consts: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct DeferredHover {
    pub base_markdown: String,
    pub range: Range,
    pub pending: PendingExpansion,
}

pub enum HoverResolution {
    Ready(Option<Hover>),
    Deferred(Box<DeferredHover>),
}

struct HoverJob {
    id: RequestId,
    deferred: DeferredHover,
}

pub struct HoverWorker {
    tx: Option<mpsc::Sender<HoverJob>>,
    handle: Option<JoinHandle<()>>,
    pub cache: ExpansionCache,
}

impl HoverWorker {
    pub fn new(tools: Arc<ToolRunner>, lsp_sender: MessageSink) -> Self {
        let cache = new_expansion_cache();
        let cache_bg = cache.clone();
        let (tx, rx) = mpsc::channel::<HoverJob>();

        let handle = thread::spawn(move || {
            for job in rx {
                let hover = finish_deferred_hover(job.deferred, &tools, &cache_bg);
                if let Ok(val) = serde_json::to_value(hover) {
                    lsp_sender(Message::Response(Response::new_ok(job.id, val)));
                }
            }
        });

        Self {
            tx: Some(tx),
            handle: Some(handle),
            cache,
        }
    }

    pub fn submit(&self, id: RequestId, deferred: DeferredHover) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(HoverJob { id, deferred });
        }
    }

    pub fn shutdown(mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn prepare_hover(
    doc: &Document,
    pos: Position,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
    tools: &ToolRunner,
    cache: &ExpansionCache,
) -> HoverResolution {
    // Handle `#include` / `.include` lines even though tree-sitter classifies `#include` as comment
    if let Some((inc_target, resolved_path)) =
        index.resolve_include_on_line(&doc.uri, pos.line, ctx)
    {
        let (ls, le, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
        let range = doc.line_index.byte_span_to_range(&doc.text, ls, le);
        let md = format!(
            "### Include `{inc_target}`\n\nResolved path: `{}`",
            resolved_path.display()
        );
        return HoverResolution::Ready(Some(make_hover(md, range)));
    }

    let file_ctx = ctx.for_uri(&doc.uri);
    let offset = doc.line_index.position_to_offset(&doc.text, pos);
    if is_inside_comment(&doc.text, doc, offset) {
        return HoverResolution::Ready(None);
    }

    let (start, end) = word_range_at(&doc.text, offset);
    if start == end {
        return HoverResolution::Ready(None);
    }
    let Some(token) = doc.text.get(start..end).map(str::to_owned) else {
        return HoverResolution::Ready(None);
    };
    let range = doc.line_index.byte_span_to_range(&doc.text, start, end);

    let (line_start, line_end, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
    let line_prefix = doc.text.get(line_start..start).unwrap_or("");
    let full_line = doc.text.get(line_start..line_end).unwrap_or("");
    let is_head = is_statement_head(line_prefix);

    let (_, option_lines) =
        effective_options_at_line(&doc.text, pos.line as usize, &file_ctx.march);

    enum Target<'a> {
        Directive(&'a DirectiveEntry),
        Instruction(&'a InstructionEntry),
        Register(&'a RegisterEntry),
        Csr(&'a CsrEntry),
        Symbol(SymbolHoverInfo),
    }

    let target = if token.starts_with('.') {
        if let Some(dir) = db.directive(&token) {
            Target::Directive(dir)
        } else if let Some(inst) = db.instruction(&token) {
            Target::Instruction(inst)
        } else if let Some(sym) = index.token_hover_info(&doc.uri, offset, ctx) {
            Target::Symbol(sym)
        } else {
            return HoverResolution::Ready(None);
        }
    } else if is_head {
        if let Some(inst) = db.instruction(&token) {
            Target::Instruction(inst)
        } else if let Some(reg) = db.register(&token) {
            Target::Register(reg)
        } else if let Some(csr) = db.csr(&token) {
            Target::Csr(csr)
        } else if let Some(sym) = index.token_hover_info(&doc.uri, offset, ctx) {
            Target::Symbol(sym)
        } else {
            return HoverResolution::Ready(None);
        }
    } else if let Some(reg) = db.register(&token) {
        Target::Register(reg)
    } else if let Some(csr) = db.csr(&token) {
        Target::Csr(csr)
    } else if let Some(sym) = index.token_hover_info(&doc.uri, offset, ctx) {
        Target::Symbol(sym)
    } else if let Some(inst) = db.instruction(&token) {
        Target::Instruction(inst)
    } else {
        return HoverResolution::Ready(None);
    };

    match target {
        Target::Directive(dir) => {
            HoverResolution::Ready(Some(make_hover(format_directive_hover(dir), range)))
        }
        Target::Register(reg) => {
            HoverResolution::Ready(Some(make_hover(format_register_hover(reg), range)))
        }
        Target::Csr(csr) => HoverResolution::Ready(Some(make_hover(format_csr_hover(csr), range))),
        Target::Symbol(sym) => {
            HoverResolution::Ready(Some(make_hover(format_symbol_hover(&sym), range)))
        }
        Target::Instruction(inst) => {
            let base_markdown = format_instruction_hover(inst, &file_ctx);
            if !inst.llvm_supported || tools.is_disabled(ToolKind::LlvmMc) {
                return HoverResolution::Ready(Some(make_hover(base_markdown, range)));
            }
            let Some(stmt) = extract_statement_at(full_line, start.saturating_sub(line_start))
            else {
                return HoverResolution::Ready(Some(make_hover(base_markdown, range)));
            };

            let known_consts = index.constant_defines_in_scope(&doc.uri, ctx);
            let mut const_pairs: Vec<(String, String)> = known_consts
                .iter()
                .filter(|(name, _)| contains_ident(&stmt, name))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            const_pairs.sort();

            let key = ExpansionCacheKey {
                stmt,
                triple: file_ctx.march.llvm_triple().to_owned(),
                mattr: file_ctx.march.to_llvm_mattr(),
                option_lines,
                const_pairs,
            };

            if let Ok(guard) = cache.0.lock()
                && let Some(cached) = guard.get(&key)
            {
                let md = append_cached_expansion(base_markdown, cached);
                return HoverResolution::Ready(Some(make_hover(md, range)));
            }

            let file_dir = uri_to_path(&doc.uri).and_then(|p| p.parent().map(Path::to_path_buf));
            let cpp_preamble = collect_cpp_preamble(&doc.text, pos.line as usize);
            HoverResolution::Deferred(Box::new(DeferredHover {
                base_markdown,
                range,
                pending: PendingExpansion {
                    key,
                    clang_flags: file_ctx.clang_flags,
                    file_dir,
                    cpp_preamble,
                    known_consts,
                },
            }))
        }
    }
}

/// Synchronous convenience wrapper for unit tests.
#[cfg(test)]
pub fn provide_hover(
    doc: &Document,
    pos: Position,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
    tools: &ToolRunner,
) -> Option<Hover> {
    let cache = new_expansion_cache();
    match prepare_hover(doc, pos, db, ctx, index, tools, &cache) {
        HoverResolution::Ready(h) => h,
        HoverResolution::Deferred(def) => Some(finish_deferred_hover(*def, tools, &cache)),
    }
}

fn finish_deferred_hover(
    deferred: DeferredHover,
    tools: &ToolRunner,
    cache: &ExpansionCache,
) -> Hover {
    if tools.is_disabled(ToolKind::LlvmMc) {
        return make_hover(deferred.base_markdown, deferred.range);
    }

    let cached = resolve_expansion(&deferred.pending, tools);
    if !tools.is_disabled(ToolKind::LlvmMc)
        && let Ok(mut guard) = cache.0.lock()
    {
        if guard.len() >= MAX_EXPANSION_CACHE_ENTRIES {
            guard.clear();
        }
        guard.insert(deferred.pending.key, cached.clone());
    }

    let md = if tools.is_disabled(ToolKind::LlvmMc) {
        deferred.base_markdown
    } else {
        append_cached_expansion(deferred.base_markdown, &cached)
    };
    make_hover(md, deferred.range)
}

fn append_cached_expansion(mut md: String, cached: &CachedExpansion) -> String {
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

fn make_hover(markdown: String, range: Range) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(range),
    }
}

fn resolve_expansion(pending: &PendingExpansion, tools: &ToolRunner) -> CachedExpansion {
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

fn parse_llvm_mc_output(stdout: &str) -> Option<Vec<ExpandedInstruction>> {
    let mut items = Vec::new();
    for raw in stdout.lines() {
        let trimmed = raw.trim();
        // Skip directives (`.text`, `.option ...`), generated labels (`.Lpcrel_hi0:`),
        // and standalone comment / relocation fixup lines (`#   fixup A - ...`) (R1).
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
    file_dir: Option<&std::path::Path>,
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

    let out = tools.run_clang(&args, &stdin_buf)?;
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

fn collect_cpp_preamble(text: &str, up_to_line: usize) -> Vec<String> {
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

fn contains_ident(haystack: &str, ident: &str) -> bool {
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

fn extract_statement_at(full_line: &str, rel_col: usize) -> Option<String> {
    let without_comment = strip_comment_from_line(full_line);
    let mut start = 0usize;
    let mut selected = without_comment;
    for seg in without_comment.split(';') {
        let seg_end = start + seg.len();
        if rel_col >= start && rel_col <= seg_end {
            selected = seg;
            break;
        }
        start = seg_end + 1;
    }
    let mut s = selected.trim();
    while let Some(colon) = s.find(':') {
        let prefix = s[..colon].trim();
        if !prefix.is_empty() && !prefix.contains(char::is_whitespace) {
            s = s[colon + 1..].trim_start();
        } else {
            break;
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

fn strip_comment_from_line(line: &str) -> &str {
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

fn is_inside_comment(text: &str, doc: &Document, offset: usize) -> bool {
    if let Some(tree) = &doc.tree {
        let probe = if offset >= text.len() && offset > 0 {
            offset - 1
        } else {
            offset
        };
        if let Some(node) = tree.root_node().descendant_for_byte_range(probe, probe)
            && node.kind() == "comment"
        {
            return true;
        }
    }
    false
}

fn is_statement_head(line_prefix: &str) -> bool {
    let stmt_part = line_prefix.rsplit(';').next().unwrap_or(line_prefix);
    let trimmed = stmt_part.trim_start();
    if trimmed.is_empty() {
        return true;
    }
    if let Some(after_colon) = trimmed.rsplit(':').next() {
        return trimmed.contains(':') && after_colon.trim().is_empty();
    }
    false
}

fn format_instruction_hover(inst: &InstructionEntry, file_ctx: &FileContext) -> String {
    let mut md = String::new();

    let xlen_matching: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| file_ctx.march.supports_xlen(v.xlen))
        .collect();
    let variants_to_show = if xlen_matching.is_empty() {
        inst.variants.iter().collect()
    } else {
        xlen_matching
    };

    let mut seen_syntax = HashSet::new();
    let mut unique_syntaxes = Vec::new();
    for v in &variants_to_show {
        let norm = normalize_syntax(&v.syntax);
        if seen_syntax.insert(norm) {
            unique_syntaxes.push(v.syntax.as_str());
        }
    }
    if unique_syntaxes.is_empty() {
        let _ = writeln!(md, "```riscv\n{}\n```", inst.mnemonic);
    } else {
        let _ = writeln!(md, "```riscv");
        for syn in &unique_syntaxes {
            let _ = writeln!(md, "{syn}");
        }
        let _ = writeln!(md, "```");
    }

    // Filter out self-references (`ld` -> `ld`) and deduplicate targets (Item 6).
    let mut seen_aliases = HashSet::new();
    let mut aliases = Vec::new();
    for v in &inst.variants {
        if let Some(alias) = v.alias_of.as_deref()
            && !alias.eq_ignore_ascii_case(&inst.mnemonic)
            && seen_aliases.insert(alias.to_ascii_lowercase())
        {
            aliases.push(alias);
        }
    }

    let mut exts = Vec::new();
    let mut has_rv32 = false;
    let mut has_rv64 = false;
    for v in &variants_to_show {
        for ext in &v.required_extensions {
            if !exts.contains(ext) {
                exts.push(ext.clone());
            }
        }
        match v.xlen {
            Xlen::Both => {
                has_rv32 = true;
                has_rv64 = true;
            }
            Xlen::Rv32 => has_rv32 = true,
            Xlen::Rv64 => has_rv64 = true,
        }
    }

    let mut meta = Vec::new();
    if let Some(long_name) = &inst.long_name {
        meta.push(format!("**{long_name}**"));
    }
    match inst.kind {
        InstructionKind::Alias => {
            if aliases.is_empty() {
                meta.push("alias".to_owned());
            } else {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!("alias of {}", targets.join(", ")));
            }
        }
        InstructionKind::Pseudo => {
            if aliases.is_empty() {
                meta.push("pseudo-instruction".to_owned());
            } else {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!(
                    "pseudo-instruction · alias of {}",
                    targets.join(", ")
                ));
            }
        }
        InstructionKind::Instruction => {
            if inst.long_name.is_none() {
                meta.push(format!("**`{}`**", inst.mnemonic));
            }
            if !aliases.is_empty() {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!("alias of {}", targets.join(", ")));
            }
        }
    }
    if !exts.is_empty() {
        let formatted: Vec<String> = exts.iter().map(|e| format!("`{e}`")).collect();
        meta.push(formatted.join(", "));
    }
    let xlen_label = match (has_rv32, has_rv64) {
        (true, false) => "RV32",
        (false, true) => "RV64",
        _ => "RV32/RV64",
    };
    meta.push(xlen_label.to_owned());
    let _ = writeln!(md, "{}", meta.join(" · "));

    if !inst.llvm_supported {
        let _ = writeln!(md, "\n> Not accepted by LLVM 19 (spec only).");
    }
    if inst.deprecated {
        if let Some(pref) = &inst.preferred_name {
            let _ = writeln!(md, "\n> Deprecated — prefer `{pref}`.");
        } else {
            let _ = writeln!(md, "\n> Deprecated.");
        }
    }

    if let Some(desc) = &inst.description {
        let first_para = desc.split("\n\n").next().unwrap_or(desc);
        let flat = first_para.split_whitespace().collect::<Vec<_>>().join(" ");
        if !flat.is_empty() {
            let _ = writeln!(md, "\n{}", truncate_chars_ellipsis(&flat, 300));
        }
    } else if inst.llvm_supported {
        let _ = writeln!(md, "\n*No description in RISC-V specification.*");
    }

    md.trim_end().to_owned()
}

fn normalize_syntax(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_csr_hover(csr: &CsrEntry) -> String {
    let mut md = String::new();
    let mut header = vec![format!("`{}`", csr.name), format!("0x{:03x}", csr.number)];
    if let Some(priv_lvl) = &csr.privilege {
        let priv_label = match priv_lvl.as_str() {
            "M" => "M-mode",
            "S" => "S-mode",
            "U" => "U-mode",
            "VS" => "VS-mode",
            "VS/HS" => "VS/HS-mode",
            "D" => "Debug",
            other => other,
        };
        header.push(priv_label.to_owned());
    }
    if !csr.llvm_supported {
        header.push("spec only".to_owned());
    }
    let _ = writeln!(md, "{}", header.join(" · "));

    if let Some(desc) = &csr.description {
        let summary = first_sentence_summary(desc);
        if !summary.is_empty() {
            let _ = writeln!(md, "\n{}", truncate_chars_ellipsis(&summary, 220));
        }
    } else if csr.llvm_supported {
        let _ = writeln!(md, "\n*No description in RISC-V specification.*");
    }

    if !csr.fields.is_empty() {
        md.push('\n');
        for f in &csr.fields {
            let bits = f.bits.replace('-', ":");
            let summary = f
                .description
                .as_deref()
                .map(first_sentence_summary)
                .unwrap_or_default();
            let truncated = truncate_chars_ellipsis(&summary, 80);
            if truncated.is_empty() {
                let _ = writeln!(md, "- `{}` [{bits}]", f.name);
            } else {
                let _ = writeln!(md, "- `{}` [{bits}] — {truncated}", f.name);
            }
        }
    }

    md.trim_end().to_owned()
}

fn format_register_hover(reg: &RegisterEntry) -> String {
    let name_part = if reg.abi_names.is_empty() {
        format!("`{}`", reg.name)
    } else {
        let abis = reg
            .abi_names
            .iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(" / ");
        format!("{abis} ({})", reg.name)
    };

    let role_part = reg
        .description
        .as_deref()
        .map(first_sentence_summary)
        .map(|s| s.trim_end_matches('.').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let cls = match reg.class {
                crate::data::RegisterClass::Integer => "integer register",
                crate::data::RegisterClass::Float => "floating-point register",
                crate::data::RegisterClass::Vector => "vector register",
            };
            format!("{cls} #{}", reg.number)
        });

    let mut parts = vec![name_part, truncate_chars_ellipsis(&role_part, 100)];
    if let Some(saver) = &reg.saver {
        parts.push(saver.clone());
    }
    parts.join(" · ")
}

fn format_directive_hover(dir: &DirectiveEntry) -> String {
    let mut md = String::new();
    if let Some(args) = &dir.arguments {
        let _ = writeln!(md, "```riscv\n{} {args}\n```", dir.name);
    } else {
        let _ = writeln!(md, "```riscv\n{}\n```", dir.name);
    }

    if !dir.llvm_supported {
        let _ = writeln!(
            md,
            "\n> Not accepted by LLVM 19 (GNU assembler / docs only)."
        );
    }

    let _ = writeln!(md, "\n{}", dir.description);
    md.trim_end().to_owned()
}

fn format_symbol_hover(info: &SymbolHoverInfo) -> String {
    let mut md = String::new();
    let kind_label = match info.kind {
        SymKind::Label => "label",
        SymKind::LocalLabel => "local label",
        SymKind::NumericLabel => "numeric label",
        SymKind::Equ => "constant (.equ)",
        SymKind::Macro => "macro",
        SymKind::MacroParam => "macro parameter",
        SymKind::Define => "#define",
    };

    let mut header = vec![format!("`{}`", info.name), kind_label.to_owned()];
    if let Some(line) = info.target_line_1based {
        if let Some(uri) = &info.target_uri {
            let file_label = uri
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_else(|| uri.as_str());
            header.push(format!("`{file_label}:{line}`"));
        } else {
            header.push(format!("line {line}"));
        }
    }
    if let Some(val) = &info.value {
        header.push(format!("`{val}`"));
    }

    let _ = writeln!(md, "{}", header.join(" · "));
    let _ = writeln!(md, "\n```riscv\n{}\n```", info.def_line);
    md.trim_end().to_owned()
}

fn first_sentence_summary(desc: &str) -> String {
    let first_para = desc.split("\n\n").next().unwrap_or(desc);
    let flat = first_para.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut in_tick = false;
    for (idx, ch) in flat.char_indices() {
        if ch == '`' {
            in_tick = !in_tick;
        } else if ch == '.' && !in_tick {
            let next_ok = flat[idx + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
            if next_ok {
                return flat[..=idx].to_owned();
            }
        }
    }
    flat
}

fn truncate_chars_ellipsis(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars || max_chars == 0 {
        return s.to_owned();
    }
    let cut_limit = max_chars.saturating_sub(1);
    let mut end_idx = s.len();
    for (count, (idx, _)) in s.char_indices().enumerate() {
        if count >= cut_limit {
            end_idx = idx;
            break;
        }
    }
    let slice = &s[..end_idx];
    let trimmed = if let Some(ws) = slice.rfind(char::is_whitespace)
        && ws > cut_limit / 2
    {
        &slice[..ws]
    } else {
        slice
    };
    format!("{}…", trimmed.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use std::str::FromStr;

    fn test_ctx_and_index() -> (CompileContext, WorkspaceIndex) {
        (
            CompileContext::new(None, Config::default()),
            WorkspaceIndex::new(None).expect("index"),
        )
    }

    #[test]
    fn hovers_registers_pseudos_csrs_and_directives() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///test.S").expect("uri");
        let doc = store.open(
            uri,
            1,
            "    la sp, _stack\n    csrw stvec, a0\n    .globl _start\n    ld a0, 0(sp)\n"
                .to_owned(),
        );
        let (mut ctx, mut index) = test_ctx_and_index();
        let tools = ToolRunner::new(
            "/nonexistent/clang".to_owned(),
            "/nonexistent/llvm-mc".to_owned(),
        );
        index.upsert_open_document(doc);

        let h_la = provide_hover(doc, Position::new(0, 4), &db, &mut ctx, &mut index, &tools)
            .expect("la hover");
        let HoverContents::Markup(m) = h_la.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("la"));
        assert!(m.value.contains("pseudo-instruction"));

        let h_sp = provide_hover(doc, Position::new(0, 7), &db, &mut ctx, &mut index, &tools)
            .expect("sp hover");
        let HoverContents::Markup(m) = h_sp.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("x2"));
        assert!(m.value.contains("callee-saved"));

        let h_csrw = provide_hover(doc, Position::new(1, 5), &db, &mut ctx, &mut index, &tools)
            .expect("csrw hover");
        let HoverContents::Markup(m) = h_csrw.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("csrrw"));

        let h_stvec = provide_hover(doc, Position::new(1, 10), &db, &mut ctx, &mut index, &tools)
            .expect("stvec hover");
        let HoverContents::Markup(m) = h_stvec.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("0x105"));
        assert!(m.value.contains("S-mode"));
        assert!(m.value.contains("- `BASE`"));
        assert!(m.value.contains("- `MODE` [1:0]"));
        assert!(!m.value.contains("| Bits |"));

        let h_globl = provide_hover(doc, Position::new(2, 5), &db, &mut ctx, &mut index, &tools)
            .expect("globl hover");
        let HoverContents::Markup(m) = h_globl.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("```riscv\n.globl"));
        assert!(!m.value.contains("*Source:"));

        // `ld` must NOT claim to be an alias of `ld` (Item 6)
        let h_ld = provide_hover(doc, Position::new(3, 5), &db, &mut ctx, &mut index, &tools)
            .expect("ld hover");
        let HoverContents::Markup(m) = h_ld.contents else {
            panic!("expected markup");
        };
        assert!(!m.value.contains("alias of `ld`"));
        assert!(!m.value.contains("**Spec syntax:**"));
        assert!(!m.value.contains("*Source:"));
    }

    #[test]
    fn hover_warns_on_doc_only_instruction() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///q.S").expect("uri");
        let doc = store.open(uri, 1, "    fadd.q f0, f1, f2\n".to_owned());
        let (mut ctx, mut index) = test_ctx_and_index();
        let tools = ToolRunner::new(
            "/nonexistent/clang".to_owned(),
            "/nonexistent/llvm-mc".to_owned(),
        );
        index.upsert_open_document(doc);
        let h = provide_hover(doc, Position::new(0, 5), &db, &mut ctx, &mut index, &tools)
            .expect("fadd.q hover");
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("Not accepted by LLVM 19"));
    }

    #[test]
    fn parse_llvm_mc_filters_fixups_and_formats_hex_and_relocated() {
        let compressed_out =
            "\t.text\n\tc.addi\tsp, -16                         # encoding: [0x41,0x11]\n";
        let items = parse_llvm_mc_output(compressed_out).expect("c.addi parsed");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].asm, "c.addi sp, -16");
        assert_eq!(items[0].encoding.as_deref(), Some("0x1141"));
        let single_md = append_cached_expansion(String::new(), &CachedExpansion::Expanded(items));
        assert_eq!(
            single_md.trim(),
            "**This line:** `c.addi sp, -16` · `0x1141`"
        );

        let csr_out =
            "\tcsrrw\tzero, stvec, t0                 # encoding: [0x73,0x90,0x52,0x10]\n";
        let csr_items = parse_llvm_mc_output(csr_out).expect("csrrw parsed");
        assert_eq!(csr_items[0].encoding.as_deref(), Some("0x10529073"));

        let la_out = "\
\t.text
.Lpcrel_hi0:
\tauipc\tt0, %pcrel_hi(sym)              # encoding: [0x97,0bAAAA0010,A,A]
                                        #   fixup A - offset: 0, value: %pcrel_hi(sym), kind: fixup_riscv_pcrel_hi20
\taddi\tt0, t0, %pcrel_lo(.Lpcrel_hi0)  # encoding: [0x93,0x82,0bAAAA0010,A]
                                        #   fixup A - offset: 0, value: %pcrel_lo(.Lpcrel_hi0), kind: fixup_riscv_pcrel_lo12_i
";
        let la_items = parse_llvm_mc_output(la_out).expect("la parsed");
        assert_eq!(la_items.len(), 2);
        assert_eq!(la_items[0].encoding.as_deref(), Some("(relocated)"));
        assert_eq!(la_items[1].encoding.as_deref(), Some("(relocated)"));
        let multi_md = append_cached_expansion(String::new(), &CachedExpansion::Expanded(la_items));
        assert!(!multi_md.contains("fixup"));
        assert!(!multi_md.contains("0bAAAA"));
        assert!(!multi_md.contains(".Lpcrel_hi0:"));
        assert!(multi_md.contains("auipc t0, %pcrel_hi(sym)  # (relocated)"));
    }
}
