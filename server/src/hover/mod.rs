mod expand;
mod format;
pub(crate) mod operands;

use std::collections::{HashMap, HashSet};
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
    CachedExpansion, ExpansionCacheKey, PendingExpansion, collect_cpp_preamble,
    format_this_line_fact, resolve_expansion,
};
use format::{
    RenderedHoverParts, build_instruction_hover_parts, format_cpp_directive_hover,
    format_include_hover, format_macro_counter_hover, format_operand_keyword_hover,
    format_symbol_hover, format_unresolved_include_hover,
};
pub(crate) use format::{
    format_csr_hover, format_directive_hover, format_instruction_completion_doc,
    format_register_hover,
};

#[derive(Clone, Default)]
pub struct ExpansionCache(Arc<Mutex<HashMap<ExpansionCacheKey, CachedExpansion>>>);

pub fn new_expansion_cache() -> ExpansionCache {
    ExpansionCache::default()
}

#[derive(Debug, Clone)]
pub struct DeferredHover {
    pub base_parts: RenderedHoverParts,
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
    in_flight: Arc<Mutex<HashSet<String>>>,
    cancelled: Arc<Mutex<HashSet<String>>>,
}

fn request_id_key(id: &RequestId) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

impl HoverWorker {
    pub fn new(tools: Arc<ToolRunner>, lsp_sender: MessageSink) -> Self {
        let cache = new_expansion_cache();
        let cache_bg = cache.clone();
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let cancelled: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let in_flight_bg = Arc::clone(&in_flight);
        let cancelled_bg = Arc::clone(&cancelled);
        let (tx, rx) = mpsc::channel::<HoverJob>();

        let handle = thread::spawn(move || {
            let mut logged_panic = false;
            for job in rx {
                let job_id = job.id.clone();
                let key = request_id_key(&job_id);
                if let Ok(mut guard) = in_flight_bg.lock() {
                    guard.remove(&key);
                }
                let was_cancelled = cancelled_bg
                    .lock()
                    .map(|mut guard| guard.remove(&key))
                    .unwrap_or(false);
                if was_cancelled {
                    lsp_sender(Message::Response(Response::new_err(
                        job_id,
                        ErrorCode::RequestCanceled as i32,
                        "request cancelled".to_owned(),
                    )));
                    continue;
                }

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
            in_flight,
            cancelled,
        }
    }

    pub fn submit(&self, id: RequestId, deferred: DeferredHover) {
        let key = request_id_key(&id);
        if let Ok(mut guard) = self.in_flight.lock() {
            guard.insert(key);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(HoverJob { id, deferred });
        }
    }

    pub fn cancel(&self, id: &RequestId) {
        let key = request_id_key(id);
        let is_pending = self
            .in_flight
            .lock()
            .map(|guard| guard.contains(&key))
            .unwrap_or(false);
        if is_pending && let Ok(mut guard) = self.cancelled.lock() {
            guard.insert(key);
        }
    }

    pub fn clear_cache(&self) {
        if let Ok(mut guard) = self.cache.0.lock() {
            guard.clear();
        }
    }

    pub fn shutdown(mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum HoverTarget<'a> {
    Directive(&'a DirectiveEntry),
    Instruction(&'a InstructionEntry),
    Register(&'a RegisterEntry),
    Csr(&'a CsrEntry),
    OperandKeyword {
        context: &'static str,
        code: String,
        prose: String,
    },
    Symbol(SymbolHoverInfo),
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
    if let Some((inc_kind, inc_target, resolved_path)) =
        index.resolve_include_on_line(&doc.uri, pos.line, ctx)
    {
        let range = doc
            .line_index
            .byte_span_to_range(&doc.text, line_start, line_end);
        let md = format_include_hover(inc_kind, &inc_target, &resolved_path.display().to_string());
        return HoverResolution::Ready(Some(make_hover(md, range)));
    } else if let Some((inc_kind, inc_target)) =
        index.parse_include_on_line(&doc.uri, pos.line, file_ctx.preprocessed)
    {
        let range = doc
            .line_index
            .byte_span_to_range(&doc.text, line_start, line_end);
        let md = format_unresolved_include_hover(inc_kind, &inc_target);
        return HoverResolution::Ready(Some(make_hover(md, range)));
    }

    if crate::line::is_offset_in_comment(
        &doc.text,
        doc.tree.as_ref(),
        offset,
        file_ctx.preprocessed,
    ) {
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
        return HoverResolution::Ready(Some(make_hover(format_macro_counter_hover(db), range)));
    }

    let starts_in_block =
        crate::line::in_block_comment_before(&doc.text, line_start, file_ctx.preprocessed);
    let line_prefix = doc.text.get(line_start..start).unwrap_or("");
    let (effective_march, option_lines) =
        effective_options_at_line(&doc.text, pos.line as usize, &file_ctx.march);
    let query = TokenTargetQuery {
        token: &token,
        line_prefix,
        starts_in_block,
        doc,
        offset,
        effective_march: &effective_march,
        db,
        ctx,
        index,
    };
    let Some(target) = resolve_token_target(query) else {
        return HoverResolution::Ready(None);
    };

    match target {
        HoverTarget::Directive(dir) => {
            HoverResolution::Ready(Some(make_hover(format_directive_hover(dir), range)))
        }
        HoverTarget::Register(reg) => {
            HoverResolution::Ready(Some(make_hover(format_register_hover(reg), range)))
        }
        HoverTarget::Csr(csr) => HoverResolution::Ready(Some(make_hover(
            format_csr_hover(csr, Some(&effective_march)),
            range,
        ))),
        HoverTarget::OperandKeyword {
            context,
            code,
            prose,
        } => HoverResolution::Ready(Some(make_hover(
            format_operand_keyword_hover(context, &code, &prose),
            range,
        ))),
        HoverTarget::Symbol(sym) => {
            HoverResolution::Ready(Some(make_hover(format_symbol_hover(&sym), range)))
        }
        HoverTarget::Instruction(inst) => prepare_instruction_hover(InstructionHoverCtx {
            inst,
            doc,
            pos,
            token_start: start,
            line_start,
            starts_in_block,
            offset,
            full_line,
            range,
            file_ctx: &file_ctx,
            effective_march: &effective_march,
            option_lines,
            db,
            ctx,
            index,
            tools,
            cache,
        }),
    }
}

struct TokenTargetQuery<'q, 'db> {
    token: &'q str,
    line_prefix: &'q str,
    starts_in_block: bool,
    doc: &'q Document,
    offset: usize,
    effective_march: &'q MarchSpec,
    db: &'db Database,
    ctx: &'q mut CompileContext,
    index: &'q mut WorkspaceIndex,
}

fn resolve_token_target<'q, 'db>(q: TokenTargetQuery<'q, 'db>) -> Option<HoverTarget<'db>> {
    let is_head = crate::line::is_statement_head_with_block_state(q.line_prefix, q.starts_in_block);
    if q.token.starts_with('.') {
        if let Some(dir) = q.db.directive(q.token) {
            Some(HoverTarget::Directive(dir))
        } else if let Some(inst) = q.db.instruction(q.token) {
            Some(HoverTarget::Instruction(inst))
        } else {
            q.index
                .token_hover_info(&q.doc.uri, q.offset, q.ctx)
                .map(HoverTarget::Symbol)
        }
    } else if is_head {
        if let Some(inst) = q.db.instruction(q.token) {
            Some(HoverTarget::Instruction(inst))
        } else if let Some(reg) = q.db.register(q.token) {
            Some(HoverTarget::Register(reg))
        } else if let Some(csr) = q.db.csr(q.token) {
            Some(HoverTarget::Csr(csr))
        } else {
            q.index
                .token_hover_info(&q.doc.uri, q.offset, q.ctx)
                .map(HoverTarget::Symbol)
        }
    } else if q.token.starts_with('%') {
        let lower = q.token.to_ascii_lowercase();
        q.db.texts
            .relocations
            .iter()
            .find(|r| r.label.trim_end_matches('(') == lower)
            .map(|reloc| HoverTarget::OperandKeyword {
                context: "Relocation modifier",
                code: reloc.label.trim_end_matches('(').to_owned(),
                prose: format!("{}.", reloc.description.trim_end_matches('.')),
            })
    } else if let Some(reg) = q.db.register(q.token) {
        Some(HoverTarget::Register(reg))
    } else if is_csr_operand_position(q.line_prefix, q.starts_in_block, q.db, q.effective_march) {
        if let Some(csr) = q.db.csr(q.token) {
            Some(HoverTarget::Csr(csr))
        } else if let Some(num) = parse_csr_number(q.token)
            && let Some(csr) = q.db.csr_by_number(num)
        {
            Some(HoverTarget::Csr(csr))
        } else {
            q.index
                .token_hover_info(&q.doc.uri, q.offset, q.ctx)
                .map(HoverTarget::Symbol)
        }
    } else if let Some(kw) = resolve_operand_keyword(
        q.token,
        q.line_prefix,
        q.starts_in_block,
        q.db,
        q.effective_march,
    ) {
        Some(kw)
    } else if let Some(sym) = q.index.token_hover_info(&q.doc.uri, q.offset, q.ctx) {
        Some(HoverTarget::Symbol(sym))
    } else if let Some(csr) = q.db.csr(q.token) {
        Some(HoverTarget::Csr(csr))
    } else {
        q.db.instruction(q.token).map(HoverTarget::Instruction)
    }
}

struct InstructionHoverCtx<'a> {
    inst: &'a InstructionEntry,
    doc: &'a Document,
    pos: Position,
    token_start: usize,
    line_start: usize,
    starts_in_block: bool,
    offset: usize,
    full_line: &'a str,
    range: Range,
    file_ctx: &'a crate::context::FileContext,
    effective_march: &'a MarchSpec,
    option_lines: Vec<String>,
    db: &'a Database,
    ctx: &'a mut CompileContext,
    index: &'a mut WorkspaceIndex,
    tools: &'a ToolRunner,
    cache: &'a ExpansionCache,
}

fn prepare_instruction_hover(c: InstructionHoverCtx<'_>) -> HoverResolution {
    let enabled_by_march = !c.inst.llvm_supported
        || c.inst.variants.iter().any(|v| {
            c.effective_march
                .supports_extensions(&v.required_extensions, v.xlen)
        });
    let mut parts =
        build_instruction_hover_parts(c.inst, c.effective_march, enabled_by_march, c.db, None);
    if !c.inst.llvm_supported || !enabled_by_march || c.tools.is_disabled(ToolKind::LlvmMc) {
        return HoverResolution::Ready(Some(make_hover(parts.render(), c.range)));
    }
    let Some(stmt) = crate::line::extract_statement_at_with_block_state(
        c.full_line,
        c.token_start.saturating_sub(c.line_start),
        c.starts_in_block,
    ) else {
        return HoverResolution::Ready(Some(make_hover(parts.render(), c.range)));
    };

    let known_consts = c
        .index
        .constant_defines_in_scope(&c.doc.uri, Some(c.offset), c.ctx);
    let mut const_pairs: Vec<(String, String)> = known_consts
        .iter()
        .filter(|(name, _)| expand::contains_ident(&stmt, name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    const_pairs.sort();

    let file_dir = uri_to_path(&c.doc.uri).and_then(|p| p.parent().map(Path::to_path_buf));
    let (cpp_preamble, open_if_depth) = if c.file_ctx.preprocessed {
        collect_cpp_preamble(&c.doc.text, c.pos.line as usize)
    } else {
        (Vec::new(), 0)
    };

    let key = ExpansionCacheKey {
        stmt,
        triple: c.file_ctx.march.llvm_triple().to_owned(),
        mattr: c.file_ctx.march.to_llvm_mattr(),
        option_lines: c.option_lines,
        const_pairs,
        clang_flags: c.file_ctx.clang_flags.clone(),
        cpp_preamble,
        open_if_depth,
        file_dir,
        working_dir: c.file_ctx.working_dir.clone(),
    };

    if let Ok(guard) = c.cache.0.lock()
        && let Some(cached) = guard.get(&key)
    {
        parts.facts.push(format_this_line_fact(cached));
        return HoverResolution::Ready(Some(make_hover(parts.render(), c.range)));
    }

    HoverResolution::Deferred(Box::new(DeferredHover {
        base_parts: parts,
        range: c.range,
        pending: PendingExpansion { key, known_consts },
    }))
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
    mut deferred: DeferredHover,
    tools: &ToolRunner,
    cache: &ExpansionCache,
) -> Hover {
    if tools.is_disabled(ToolKind::LlvmMc) {
        return make_hover(deferred.base_parts.render(), deferred.range);
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

    if !tools.is_disabled(ToolKind::LlvmMc) {
        deferred
            .base_parts
            .facts
            .push(format_this_line_fact(&cached));
    }
    make_hover(deferred.base_parts.render(), deferred.range)
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

pub(crate) fn parse_csr_number(token: &str) -> Option<u16> {
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

fn is_csr_operand_position(
    line_prefix: &str,
    starts_in_block: bool,
    db: &Database,
    march: &MarchSpec,
) -> bool {
    has_operand_class_at(line_prefix, starts_in_block, db, march, |c| {
        c == "csr_sysreg"
    })
}

fn has_operand_class_at(
    line_prefix: &str,
    starts_in_block: bool,
    db: &Database,
    march: &MarchSpec,
    pred: impl Fn(&str) -> bool,
) -> bool {
    let stmt = crate::line::active_statement_prefix_with_block_state(line_prefix, starts_in_block);
    let Some(ws) = stmt.find(char::is_whitespace) else {
        return false;
    };
    let head = stmt[..ws].trim();
    let args = stmt[ws..].trim_start();
    let Some(inst) = db.instruction(head) else {
        return false;
    };

    let (op_idx, inside_parens) = crate::line::count_operand_position(args);
    if inside_parens {
        return false;
    }

    let mut matched_march = false;
    for v in &inst.variants {
        if march.supports_extensions(&v.required_extensions, v.xlen) {
            matched_march = true;
            if v.operands.get(op_idx).is_some_and(|op| pred(&op.class)) {
                return true;
            }
        }
    }
    if !matched_march {
        return inst
            .variants
            .iter()
            .any(|v| v.operands.get(op_idx).is_some_and(|op| pred(&op.class)));
    }
    false
}

fn resolve_operand_keyword<'a>(
    token: &str,
    line_prefix: &str,
    starts_in_block: bool,
    db: &Database,
    march: &MarchSpec,
) -> Option<HoverTarget<'a>> {
    let lower = token.to_ascii_lowercase();
    if lower == "v0.t"
        && has_operand_class_at(line_prefix, starts_in_block, db, march, |c| c == "VMaskOp")
    {
        return Some(HoverTarget::OperandKeyword {
            context: "Vector mask",
            code: "v0.t".to_owned(),
            prose:
                "Predicated vector execution mask using `v0` (element active where mask bit = 1)."
                    .to_owned(),
        });
    }
    if has_operand_class_at(line_prefix, starts_in_block, db, march, |c| {
        c.starts_with("frmarg") || c == "rtzarg"
    }) && let Some((name, desc)) = crate::completion::candidates::ROUNDING_MODES
        .iter()
        .find(|(name, _)| *name == lower)
    {
        return Some(HoverTarget::OperandKeyword {
            context: "Rounding mode",
            code: (*name).to_owned(),
            prose: format!("{desc}."),
        });
    }
    if has_operand_class_at(line_prefix, starts_in_block, db, march, |c| c == "fencearg") {
        if let Some((name, desc)) = crate::completion::candidates::FENCE_ARGS
            .iter()
            .find(|(name, _)| *name == lower)
        {
            return Some(HoverTarget::OperandKeyword {
                context: "Fence ordering",
                code: (*name).to_owned(),
                prose: format!("{desc}."),
            });
        }
        if !lower.is_empty()
            && lower
                .bytes()
                .all(|b| matches!(b, b'i' | b'o' | b'r' | b'w'))
        {
            let parts: Vec<&str> = lower
                .bytes()
                .map(|b| match b {
                    b'i' => "Input",
                    b'o' => "Output",
                    b'r' => "Read",
                    b'w' => "Write",
                    _ => unreachable!(),
                })
                .collect();
            return Some(HoverTarget::OperandKeyword {
                context: "Fence ordering",
                code: lower,
                prose: format!("{}.", parts.join(", ")),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use expand::{ExpandedInstruction, parse_llvm_mc_output};

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
        let uri = crate::context::test_uri("hover_test.S");
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
        assert!(m.value.contains("Pseudo-instruction"));

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
        assert!(m.value.contains("stvec"));
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

        // `ld` shows `RV64I` base and never claims self-alias
        let h_ld = provide_hover(doc, Position::new(3, 5), &db, &mut ctx, &mut index, &tools)
            .expect("ld hover");
        let HoverContents::Markup(m) = h_ld.contents else {
            panic!("expected markup");
        };
        assert!(!m.value.contains("Alias of: `ld`"));
        assert!(m.value.contains("`RV64I`"));
        assert!(m.value.contains("Operands:"));
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
        let uri = crate::context::test_uri("hover_disabled.S");
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
        let uri = crate::context::test_uri("hover_defs.S");
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
        let uri = crate::context::test_uri("hover_q.S");
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
        let single_md = format_this_line_fact(&CachedExpansion::Expanded(items));
        assert_eq!(single_md.trim(), "This line: `c.addi sp, -16` → `0x1141`");

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
        let multi_md = format_this_line_fact(&CachedExpansion::Expanded(la_items));
        assert!(!multi_md.contains("fixup"));
        assert!(!multi_md.contains("0bAAAA"));
        assert!(!multi_md.contains(".Lpcrel_hi0:"));
        assert!(multi_md.contains("auipc t0, %pcrel_hi(sym)  # (relocated)"));
    }

    #[test]
    fn hover_handles_numeric_csr_set_reassignment_irp_and_macro_counter() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("hover_extra.S");
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
        assert!(m.value.contains("Symbol (.set)"), "got: {}", m.value);
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
        assert!(m.value.contains("stvec") && m.value.contains("0x105"));

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

    #[test]
    fn cancel_skips_pending_hover_job() {
        let (tx_out, rx_out) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started_tx = Mutex::new(Some(started_tx));
        let release_rx = Mutex::new(Some(release_rx));
        let tools = Arc::new(ToolRunner::new("clang".to_owned(), "llvm-mc".to_owned()));
        let worker = HoverWorker::new(
            tools,
            Arc::new(move |msg| {
                // Hold the worker on job 1 until job 2 is submitted and cancelled.
                if let Message::Response(r) = &msg
                    && r.id == RequestId::from(1)
                {
                    if let Ok(mut guard) = started_tx.lock()
                        && let Some(tx) = guard.take()
                    {
                        let _ = tx.send(());
                    }
                    if let Ok(mut guard) = release_rx.lock()
                        && let Some(rx) = guard.take()
                    {
                        let _ = rx.recv();
                    }
                }
                let _ = tx_out.send(msg);
            }),
        );
        let dummy = DeferredHover {
            base_parts: RenderedHoverParts {
                context: String::new(),
                lang: "riscv",
                code: String::new(),
                below_fence: None,
                notices: Vec::new(),
                facts: Vec::new(),
                prose: None,
            },
            range: Range::default(),
            pending: PendingExpansion {
                key: ExpansionCacheKey {
                    stmt: "nop".to_owned(),
                    triple: "riscv64".to_owned(),
                    mattr: String::new(),
                    option_lines: Vec::new(),
                    const_pairs: Vec::new(),
                    clang_flags: Vec::new(),
                    cpp_preamble: Vec::new(),
                    open_if_depth: 0,
                    file_dir: None,
                    working_dir: None,
                },
                known_consts: HashMap::new(),
            },
        };
        let blocker_id = RequestId::from(1);
        let id = RequestId::from(99);
        worker.cancel(&id); // canceling unknown id is a no-op
        worker.submit(blocker_id, dummy.clone());
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocker job started");
        worker.submit(id.clone(), dummy);
        worker.cancel(&id);
        let _ = release_tx.send(());

        let _ = rx_out
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocker response");
        let msg = rx_out
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cancelled response");
        let Message::Response(resp) = msg else {
            panic!("expected response");
        };
        assert_eq!(resp.id, id);
        let err = resp
            .response_result
            .expect_err("expected RequestCanceled error");
        assert_eq!(err.code, ErrorCode::RequestCanceled as i32);
        worker.shutdown();
    }
}
