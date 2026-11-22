mod expand;
mod format;

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use lsp_server::{ErrorCode, Message, RequestId, Response};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};

use crate::context::{CompileContext, MarchSpec, effective_options_at_line, uri_to_path};
use crate::data::{CsrEntry, Database, DirectiveEntry, InstructionEntry, RegisterEntry};
use crate::diagnostics::MessageSink;
use crate::document::Document;
use crate::index::{SymbolHoverInfo, WorkspaceIndex};
use crate::position::word_range_at;
use crate::preprocessor::{HashLineKind, classify_hash_line};
use crate::tools::{ToolKind, ToolRunner};

use expand::{
    CachedExpansion, ExpansionCacheKey, PendingExpansion, append_cached_expansion,
    collect_cpp_preamble, resolve_expansion,
};
use format::{
    format_cpp_directive_hover, format_csr_hover, format_directive_hover, format_instruction_hover,
    format_register_hover, format_symbol_hover,
};

#[derive(Clone, Default)]
pub struct ExpansionCache(Arc<Mutex<HashMap<ExpansionCacheKey, CachedExpansion>>>);

pub fn new_expansion_cache() -> ExpansionCache {
    ExpansionCache::default()
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
            let mut logged_panic = false;
            for job in rx {
                let job_id = job.id.clone();
                match catch_unwind(AssertUnwindSafe(|| {
                    finish_deferred_hover(job.deferred, &tools, &cache_bg)
                })) {
                    Ok(hover) => {
                        if let Ok(val) = serde_json::to_value(hover) {
                            lsp_sender(Message::Response(Response::new_ok(job_id, val)));
                        }
                    }
                    Err(payload) => {
                        if !logged_panic {
                            logged_panic = true;
                            let msg = crate::server::panic_payload_message(&*payload);
                            eprintln!("riscv-asm-lsp: panic in hover worker: {msg}");
                        }
                        lsp_sender(Message::Response(Response::new_err(
                            job_id,
                            ErrorCode::InternalError as i32,
                            "internal error during hover expansion".to_owned(),
                        )));
                    }
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
    let file_ctx = ctx.for_uri(&doc.uri);
    index.sync_preprocessed(&doc.uri, file_ctx.preprocessed);
    let offset = doc.line_index.position_to_offset(&doc.text, pos);
    let (line_start, line_end, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
    let full_line = doc.text.get(line_start..line_end).unwrap_or("");

    // Hover on `#<directive>` keyword in preprocessed files before `#include` path resolution
    if file_ctx.preprocessed
        && let Some(HashLineKind::Directive {
            hash_offset,
            name,
            name_span,
            ..
        }) = classify_hash_line(full_line)
        && offset >= line_start + hash_offset
        && offset <= line_start + name_span.1
    {
        let range = doc.line_index.byte_span_to_range(
            &doc.text,
            line_start + hash_offset,
            line_start + name_span.1,
        );
        return HoverResolution::Ready(Some(make_hover(format_cpp_directive_hover(name), range)));
    }

    // Handle `#include` / `.include` lines even though tree-sitter classifies `#include` as comment
    if let Some((inc_target, resolved_path)) =
        index.resolve_include_on_line(&doc.uri, pos.line, ctx)
    {
        let range = doc
            .line_index
            .byte_span_to_range(&doc.text, line_start, line_end);
        let md = format!(
            "### Include `{inc_target}`\n\nResolved path: `{}`",
            resolved_path.display()
        );
        return HoverResolution::Ready(Some(make_hover(md, range)));
    }

    if is_inside_comment(&doc.text, doc, offset, file_ctx.preprocessed) {
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

    if token == "\\@" {
        return HoverResolution::Ready(Some(make_hover(
            "### Macro invocation counter (`\\@`)\n\nExpands to the number of macros executed so far, useful for generating unique local labels such as `loop_\\@:` inside `.macro` bodies.".to_owned(),
            range,
        )));
    }

    let line_prefix = doc.text.get(line_start..start).unwrap_or("");
    let is_head = is_statement_head(line_prefix);

    let (effective_march, option_lines) =
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
    } else if is_csr_operand_position(line_prefix, db, &effective_march)
        && let Some(num) = parse_csr_number(&token)
        && let Some(csr) = db.csr_by_number(num)
    {
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
            let enabled_by_march = !inst.llvm_supported
                || inst
                    .variants
                    .iter()
                    .any(|v| effective_march.supports_extensions(&v.required_extensions, v.xlen));
            let base_markdown = format_instruction_hover(inst, &effective_march, enabled_by_march);
            if !inst.llvm_supported || !enabled_by_march || tools.is_disabled(ToolKind::LlvmMc) {
                return HoverResolution::Ready(Some(make_hover(base_markdown, range)));
            }
            let Some(stmt) = extract_statement_at(full_line, start.saturating_sub(line_start))
            else {
                return HoverResolution::Ready(Some(make_hover(base_markdown, range)));
            };

            let known_consts = index.constant_defines_in_scope(&doc.uri, Some(offset), ctx);
            let mut const_pairs: Vec<(String, String)> = known_consts
                .iter()
                .filter(|(name, _)| expand::contains_ident(&stmt, name))
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
            let cpp_preamble = if file_ctx.preprocessed {
                collect_cpp_preamble(&doc.text, pos.line as usize)
            } else {
                Vec::new()
            };
            HoverResolution::Deferred(Box::new(DeferredHover {
                base_markdown,
                range,
                pending: PendingExpansion {
                    key,
                    clang_flags: file_ctx.clang_flags,
                    file_dir,
                    working_dir: file_ctx.working_dir,
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
        if guard.len() >= expand::MAX_EXPANSION_CACHE_ENTRIES {
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

fn make_hover(markdown: String, range: Range) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(range),
    }
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

fn is_inside_comment(text: &str, doc: &Document, offset: usize, preprocessed: bool) -> bool {
    if let Some(tree) = &doc.tree {
        let probe = if offset >= text.len() && offset > 0 {
            offset - 1
        } else {
            offset
        };
        if let Some(node) = tree.root_node().descendant_for_byte_range(probe, probe)
            && node.kind() == "comment"
        {
            if preprocessed
                && let Ok(raw) = node.utf8_text(text.as_bytes())
                && matches!(
                    classify_hash_line(raw),
                    Some(HashLineKind::Directive { .. })
                )
            {
                return false;
            }
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

fn parse_csr_number(token: &str) -> Option<u16> {
    let val = if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16).ok()?
    } else if let Some(bin) = token
        .strip_prefix("0b")
        .or_else(|| token.strip_prefix("0B"))
    {
        u16::from_str_radix(bin, 2).ok()?
    } else if token.starts_with('0') && token.len() > 1 {
        u16::from_str_radix(&token[1..], 8).ok()?
    } else {
        token.parse::<u16>().ok()?
    };
    (val <= 0xfff).then_some(val)
}

fn is_csr_operand_position(line_prefix: &str, db: &Database, march: &MarchSpec) -> bool {
    let mut stmt = line_prefix
        .rsplit(';')
        .next()
        .unwrap_or(line_prefix)
        .trim_start();
    while let Some(colon) = stmt.find(':') {
        let label = stmt[..colon].trim();
        if !label.is_empty() && !label.contains(char::is_whitespace) {
            stmt = stmt[colon + 1..].trim_start();
        } else {
            break;
        }
    }
    let Some(ws) = stmt.find(char::is_whitespace) else {
        return false;
    };
    let head = stmt[..ws].trim();
    let args = stmt[ws..].trim_start();
    let Some(inst) = db.instruction(head) else {
        return false;
    };

    let mut depth = 0usize;
    let mut op_idx = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
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
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => op_idx += 1,
                _ => {}
            }
        }
    }
    if depth > 0 {
        return false;
    }

    let mut matched_march = false;
    for v in &inst.variants {
        if march.supports_extensions(&v.required_extensions, v.xlen) {
            matched_march = true;
            if v.operands
                .get(op_idx)
                .is_some_and(|op| op.class == "csr_sysreg")
            {
                return true;
            }
        }
    }
    if !matched_march {
        return inst.variants.iter().any(|v| {
            v.operands
                .get(op_idx)
                .is_some_and(|op| op.class == "csr_sysreg")
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use expand::{ExpandedInstruction, parse_llvm_mc_output};
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
            "    la sp, _stack\n    csrw stvec, a0\n    .globl _start\n    ld a0, 0(sp)\n    addi a0, a1, 1\n"
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

        // `ld` shows `RV64I` base and never claims self-alias (Item 6 + Review Item 6)
        let h_ld = provide_hover(doc, Position::new(3, 5), &db, &mut ctx, &mut index, &tools)
            .expect("ld hover");
        let HoverContents::Markup(m) = h_ld.contents else {
            panic!("expected markup");
        };
        assert!(!m.value.contains("alias of `ld`"));
        assert!(m.value.contains("`RV64I`"));
        assert!(!m.value.contains("**Spec syntax:**"));
        assert!(!m.value.contains("*Source:"));

        // `addi` shows `I` base
        let h_addi = provide_hover(doc, Position::new(4, 5), &db, &mut ctx, &mut index, &tools)
            .expect("addi hover");
        let HoverContents::Markup(m) = h_addi.contents else {
            panic!("expected markup");
        };
        assert!(m.value.contains("`I` · RV32/RV64"));
    }

    #[test]
    fn hover_explains_march_disabled_and_formats_ext_alternatives() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///disabled.S").expect("uri");
        let doc = store.open(
            uri,
            1,
            "    vadd.vv v1, v2, v3\n    add.uw a0, a1, a2\n.option arch, +v\n    vadd.vv v1, v2, v3\n"
                .to_owned(),
        );
        let (mut ctx, mut index) = test_ctx_and_index();
        let tools = ToolRunner::new(
            "/nonexistent/clang".to_owned(),
            "/nonexistent/llvm-mc".to_owned(),
        );
        index.upsert_open_document(doc);

        let h_vadd = provide_hover(doc, Position::new(0, 5), &db, &mut ctx, &mut index, &tools)
            .expect("vadd.vv hover");
        let HoverContents::Markup(m) = h_vadd.contents else {
            panic!();
        };
        assert!(m.value.contains("V (or Zve32x)"), "got: {}", m.value);
        assert!(
            m.value.contains("not enabled by `-march=rv64gc`"),
            "got: {}",
            m.value
        );
        assert!(!m.value.contains("v|zve32x"), "raw pipe in: {}", m.value);
        assert!(!m.value.contains("expansion unavailable"));

        let h_adduw = provide_hover(doc, Position::new(1, 5), &db, &mut ctx, &mut index, &tools)
            .expect("add.uw hover");
        let HoverContents::Markup(m) = h_adduw.contents else {
            panic!();
        };
        assert!(m.value.contains("Zba"), "got: {}", m.value);
        assert!(m.value.contains("not enabled by `-march=rv64gc`"));

        // After `.option arch, +v`, `vadd.vv` is enabled and shows no disabled notice
        let h_vadd_ok = provide_hover(doc, Position::new(3, 5), &db, &mut ctx, &mut index, &tools)
            .expect("vadd.vv after option arch +v");
        let HoverContents::Markup(m) = h_vadd_ok.contents else {
            panic!();
        };
        assert!(!m.value.contains("not enabled by"));
    }

    #[test]
    fn hover_works_for_define_in_assembly_file() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///defs.S").expect("uri");
        let doc = store.open(
            uri,
            1,
            "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n".to_owned(),
        );
        let (mut ctx, mut index) = test_ctx_and_index();
        let tools = ToolRunner::new(
            "/nonexistent/clang".to_owned(),
            "/nonexistent/llvm-mc".to_owned(),
        );
        index.upsert_open_document(doc);

        let h_use = provide_hover(doc, Position::new(2, 12), &db, &mut ctx, &mut index, &tools)
            .expect("LOCAL_VALUE usage hover");
        let HoverContents::Markup(m) = h_use.contents else {
            panic!();
        };
        assert!(m.value.contains("LOCAL_VALUE") && m.value.contains("42"));

        let h_def = provide_hover(doc, Position::new(0, 10), &db, &mut ctx, &mut index, &tools)
            .expect("LOCAL_VALUE def hover");
        let HoverContents::Markup(m) = h_def.contents else {
            panic!();
        };
        assert!(m.value.contains("LOCAL_VALUE") && m.value.contains("42"));
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
        let items: Vec<ExpandedInstruction> =
            parse_llvm_mc_output(compressed_out).expect("c.addi parsed");
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

    #[test]
    fn hover_handles_numeric_csr_set_reassignment_irp_and_macro_counter() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///extra.S").expect("uri");
        let src = "\
.set CNT, 1
    li t0, CNT
.set CNT, CNT + 1
    li t1, CNT
    csrr a0, 0x105
    addi a0, zero, 0x105
.macro UNIQ
loop_\\@:
.endm
.irp r, a0, a1
    mv \\r, zero
.endr
";
        let doc = store.open(uri, 1, src.to_owned());
        let (mut ctx, mut index) = test_ctx_and_index();
        let tools = ToolRunner::new(
            "/nonexistent/clang".to_owned(),
            "/nonexistent/llvm-mc".to_owned(),
        );
        index.upsert_open_document(doc);

        let h_before = provide_hover(doc, Position::new(1, 12), &db, &mut ctx, &mut index, &tools)
            .expect("CNT before");
        let HoverContents::Markup(m) = h_before.contents else {
            panic!();
        };
        assert!(m.value.contains("symbol (.set)"), "got: {}", m.value);
        assert!(m.value.contains("`1`"), "got: {}", m.value);
        assert!(m.value.contains("Assigned 2 times"), "got: {}", m.value);

        let h_after = provide_hover(doc, Position::new(3, 12), &db, &mut ctx, &mut index, &tools)
            .expect("CNT after");
        let HoverContents::Markup(m) = h_after.contents else {
            panic!();
        };
        assert!(m.value.contains("`CNT + 1`"), "got: {}", m.value);

        let h_csr_num = provide_hover(doc, Position::new(4, 15), &db, &mut ctx, &mut index, &tools)
            .expect("numeric CSR 0x105");
        let HoverContents::Markup(m) = h_csr_num.contents else {
            panic!();
        };
        assert!(m.value.contains("`stvec`") && m.value.contains("0x105"));

        // Immediate operand on `addi` must NOT resolve as CSR 0x105
        assert!(
            provide_hover(doc, Position::new(5, 21), &db, &mut ctx, &mut index, &tools).is_none()
        );

        let h_counter = provide_hover(doc, Position::new(7, 6), &db, &mut ctx, &mut index, &tools)
            .expect("\\@ hover");
        let HoverContents::Markup(m) = h_counter.contents else {
            panic!();
        };
        assert!(m.value.contains("Macro invocation counter"));

        let h_irp = provide_hover(doc, Position::new(10, 8), &db, &mut ctx, &mut index, &tools)
            .expect("\\r hover");
        let HoverContents::Markup(m) = h_irp.contents else {
            panic!();
        };
        assert!(m.value.contains(".irp parameter"));
    }
}
