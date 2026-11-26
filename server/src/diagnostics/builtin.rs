use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::{
    Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Position, Range, Uri,
};

use crate::context::{FileContext, MarchSpec, path_key, uri_to_path};
use crate::data::{Database, InstructionVariant, RegisterClass, Xlen};
use crate::hover::operands::parse_immediate_spec;
use crate::position::LineIndex;
use crate::preprocessor::{
    HashLineKind, LineState, PreprocessOptions, PreprocessedLine, classify_hash_line,
    preprocess_document,
};

use super::StoredDiagnostic;

const MAX_ASM_INCLUDE_DEPTH: usize = 16;
const MAX_ASM_INCLUDE_BYTES: u64 = 2 * 1024 * 1024;
const SOURCE_NAME: &str = "riscv-asm-lsp";

pub mod codes {
    pub const UNKNOWN_MNEMONIC: &str = "unknown-mnemonic";
    pub const UNKNOWN_DIRECTIVE: &str = "unknown-directive";
    pub const EXTENSION_DISABLED: &str = "extension-disabled";
    pub const XLEN_MISMATCH: &str = "xlen-mismatch";
    pub const OPERAND_COUNT: &str = "operand-count";
    pub const INVALID_REGISTER: &str = "invalid-register";
    pub const INVALID_IMMEDIATE: &str = "invalid-immediate";
    pub const INVALID_CSR: &str = "invalid-csr";
    pub const GNU_ONLY_DIRECTIVE: &str = "gnu-only-directive";
    pub const INACTIVE_BRANCH: &str = "inactive-branch";
}

/// Validate a document purely in Rust using the built-in preprocessor output and bundled
/// instruction/CSR/directive database. Skips uncertain/macro/symbolic constructs rather than
/// risking false positives against `llvm-mc`.
pub fn run_builtin_diagnostics(
    uri: &Uri,
    text: &str,
    file_ctx: &FileContext,
    db: &Database,
    suppress: &[String],
) -> Vec<StoredDiagnostic> {
    if !file_ctx.is_riscv {
        return Vec::new();
    }
    let file_path = uri_to_path(uri);
    let preproc = preprocess_document(
        text,
        &PreprocessOptions {
            file_path: file_path.as_deref(),
            file_ctx,
            db,
            open_files: None,
        },
    );

    let line_index = LineIndex::new(text);
    let mut state = ValidatorState::new(
        file_ctx.march.clone(),
        preproc.has_unresolved_include,
        file_path.as_deref(),
        file_ctx,
    );
    let mut out = Vec::new();

    for pl in &preproc.lines {
        if pl.state != LineState::Active {
            continue;
        }
        // Scan `.macro`/`.irp`/`.irpc`/`.rept`/`.if`/`.option`/`.equ`/`.include` even if `has_unexpanded_macro`
        // might be set only for call lines, EXCEPT skip instruction diagnostics when `has_unexpanded_macro` is set.
        let statements = split_statements_with_spans(&pl.text);
        for (stmt_start, stmt_end) in statements {
            let Some(stmt) = pl.text.get(stmt_start..stmt_end) else {
                continue;
            };
            validate_statement(
                stmt,
                stmt_start,
                pl,
                text,
                &line_index,
                db,
                &mut state,
                &mut out,
            );
        }
    }

    if file_ctx.preprocessed {
        emit_inactive_branch_hints(&preproc.lines, text, &line_index, &mut out);
    }

    filter_suppressed(out, suppress)
}

fn emit_inactive_branch_hints(
    lines: &[PreprocessedLine],
    text: &str,
    line_index: &LineIndex,
    out: &mut Vec<StoredDiagnostic>,
) {
    let mut inactive_lines: Vec<usize> = Vec::new();
    for pl in lines {
        if pl.state == LineState::Inactive {
            for ln in pl.source_line..=pl.end_source_line {
                inactive_lines.push(ln);
            }
        }
    }
    inactive_lines.sort_unstable();
    inactive_lines.dedup();

    let mut run_start: Option<usize> = None;
    let mut run_last_nonempty: Option<usize> = None;
    let mut prev_line: Option<usize> = None;

    let flush =
        |start_opt: Option<usize>, end_opt: Option<usize>, out: &mut Vec<StoredDiagnostic>| {
            let (Some(start_ln), Some(end_ln)) = (start_opt, end_opt) else {
                return;
            };
            let (_, end_byte, _) = line_index.line_bounds(text, end_ln);
            let end_pos = line_index.offset_to_position(text, end_byte);
            let range = Range::new(Position::new(start_ln as u32, 0), end_pos);
            out.push(StoredDiagnostic {
                diagnostic: Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::HINT),
                    code: Some(NumberOrString::String(codes::INACTIVE_BRANCH.to_owned())),
                    code_description: None,
                    source: Some(SOURCE_NAME.to_owned()),
                    message: "Inactive preprocessor branch".to_owned(),
                    related_information: None,
                    tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                    data: None,
                },
                suggestions: Vec::new(),
                macro_def_hints: Vec::new(),
            });
        };

    for ln in inactive_lines {
        let is_contiguous = prev_line.is_some_and(|p| ln <= p + 1);
        if !is_contiguous {
            flush(run_start, run_last_nonempty, out);
            run_start = None;
            run_last_nonempty = None;
        }
        prev_line = Some(ln);

        let (ls, ce, _) = line_index.line_bounds(text, ln);
        let non_empty = !text.get(ls..ce).unwrap_or("").trim().is_empty();
        if non_empty {
            if run_start.is_none() {
                run_start = Some(ln);
            }
            run_last_nonempty = Some(ln);
        }
    }
    flush(run_start, run_last_nonempty, out);
}

pub(super) fn filter_suppressed(
    diags: Vec<StoredDiagnostic>,
    suppress: &[String],
) -> Vec<StoredDiagnostic> {
    if suppress.is_empty() {
        return diags;
    }
    diags
        .into_iter()
        .filter(|d| {
            let Some(NumberOrString::String(code)) = &d.diagnostic.code else {
                return true;
            };
            !suppress.iter().any(|s| s == code)
        })
        .collect()
}

struct ValidatorState<'a> {
    march: MarchSpec,
    option_stack: Vec<MarchSpec>,
    macro_depth: usize,
    asm_if_stack: Vec<AsmIfState>,
    known_macros: HashSet<String>,
    equ_consts: HashMap<String, Option<i128>>,
    has_unresolved_cpp_include: bool,
    has_unresolved_asm_include: bool,
    current_file: Option<&'a Path>,
    file_ctx: &'a FileContext,
    active_includes: HashSet<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsmIfState {
    Active,
    Inactive,
    Uncertain,
}

impl<'a> ValidatorState<'a> {
    fn new(
        march: MarchSpec,
        has_unresolved_cpp_include: bool,
        current_file: Option<&'a Path>,
        file_ctx: &'a FileContext,
    ) -> Self {
        let mut active_includes = HashSet::new();
        if let Some(p) = current_file {
            active_includes.insert(path_key(p));
        }
        Self {
            march,
            option_stack: Vec::new(),
            macro_depth: 0,
            asm_if_stack: Vec::new(),
            known_macros: HashSet::new(),
            equ_consts: HashMap::new(),
            has_unresolved_cpp_include,
            has_unresolved_asm_include: false,
            current_file,
            file_ctx,
            active_includes,
        }
    }

    fn in_inactive_asm_if(&self) -> bool {
        self.asm_if_stack.iter().any(|s| *s != AsmIfState::Active)
    }

    fn has_any_unresolved_include(&self) -> bool {
        self.has_unresolved_cpp_include || self.has_unresolved_asm_include
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_statement(
    stmt: &str,
    stmt_base: usize,
    pl: &PreprocessedLine,
    source_text: &str,
    line_index: &LineIndex,
    db: &Database,
    state: &mut ValidatorState<'_>,
    out: &mut Vec<StoredDiagnostic>,
) {
    let trimmed_rel = strip_leading_labels_rel(stmt);
    let Some(trimmed) = stmt.get(trimmed_rel.0..trimmed_rel.1) else {
        return;
    };
    let abs_start = stmt_base + trimmed_rel.0;
    let abs_end = stmt_base + trimmed_rel.1;
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return;
    }

    // Track `.macro`/`.irp`/`.irpc`/`.rept`/`.endm`/`.endr` nesting first.
    if let Some((head, rest)) = split_head_and_rest(trimmed) {
        let head_lower = head.to_ascii_lowercase();
        match head_lower.as_str() {
            ".macro" => {
                if state.macro_depth == 0
                    && !state.in_inactive_asm_if()
                    && let Some(name) = rest
                        .trim_start()
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '(')
                        .next()
                        .filter(|s| !s.is_empty())
                {
                    state.known_macros.insert(name.to_ascii_lowercase());
                }
                state.macro_depth += 1;
                return;
            }
            ".irp" | ".irpc" | ".rept" => {
                state.macro_depth += 1;
                return;
            }
            ".endm" | ".endr" => {
                state.macro_depth = state.macro_depth.saturating_sub(1);
                return;
            }
            _ => {}
        }

        if state.macro_depth > 0 {
            return;
        }

        // Track assembler conditional blocks (`.if`, `.ifdef`, `.ifndef`, `.else`, `.elseif`, `.endif`).
        match head_lower.as_str() {
            ".if" | ".ifeqs" | ".ifnes" | ".ifb" | ".ifnb" | ".ifc" | ".ifnc" | ".ifdef"
            | ".ifndef" => {
                let cond = if state.in_inactive_asm_if() {
                    AsmIfState::Inactive
                } else if head_lower == ".if" {
                    match eval_asm_const_expr(rest.trim(), &state.equ_consts) {
                        Some(0) => AsmIfState::Inactive,
                        Some(_) => AsmIfState::Active,
                        None => AsmIfState::Uncertain,
                    }
                } else {
                    AsmIfState::Uncertain
                };
                state.asm_if_stack.push(cond);
                return;
            }
            ".elseif" | ".else" => {
                if let Some(top) = state.asm_if_stack.last_mut() {
                    *top = match *top {
                        AsmIfState::Active => AsmIfState::Inactive,
                        AsmIfState::Inactive if head_lower == ".else" => AsmIfState::Active,
                        AsmIfState::Inactive => {
                            match eval_asm_const_expr(rest.trim(), &state.equ_consts) {
                                Some(0) => AsmIfState::Inactive,
                                Some(_) => AsmIfState::Active,
                                None => AsmIfState::Uncertain,
                            }
                        }
                        AsmIfState::Uncertain => AsmIfState::Uncertain,
                    };
                }
                return;
            }
            ".endif" => {
                state.asm_if_stack.pop();
                return;
            }
            _ => {}
        }

        if state.in_inactive_asm_if() {
            return;
        }

        // Handle assignment statement `NAME = EXPR`.
        if is_symbol_assignment(trimmed) {
            update_assignment_const(trimmed, &mut state.equ_consts);
            return;
        }

        let head_range = map_expanded_range(
            source_text,
            line_index,
            pl,
            abs_start,
            abs_start + head.len(),
        );
        let stmt_range = map_expanded_range(source_text, line_index, pl, abs_start, abs_end);

        if head_lower.starts_with('.') {
            handle_directive_statement(&head_lower, head, rest, head_range, db, state, out, 0);
            return;
        }

        // Skip C preprocessor function-like macro invocations (e.g. `SET_FAULT_HANDLER(x0, a1)`).
        if head.contains('(') || rest.trim_start().starts_with('(') {
            return;
        }

        if pl.has_unexpanded_macro {
            return;
        }

        validate_instruction_statement(
            &head_lower,
            head,
            &trimmed[head.len()..],
            abs_start,
            head.len(),
            head_range,
            stmt_range,
            pl,
            source_text,
            line_index,
            db,
            state,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_directive_statement(
    head_lower: &str,
    head_raw: &str,
    rest: &str,
    head_range: Range,
    db: &Database,
    state: &mut ValidatorState<'_>,
    out: &mut Vec<StoredDiagnostic>,
    include_depth: usize,
) {
    match head_lower {
        ".insn" => return,
        ".option" => apply_option_directive(rest, state),
        ".attribute" => {
            if let Some(arch) = crate::context::parse_attribute_arch(rest) {
                state.march.apply_option_arch(arch);
            }
        }
        ".equ" | ".equiv" | ".set" => {
            if let Some((sym, expr)) = rest.split_once(',') {
                let sym_name = sym.trim();
                if !sym_name.is_empty() {
                    let val = eval_asm_const_expr(expr.trim(), &state.equ_consts);
                    state.equ_consts.insert(sym_name.to_owned(), val);
                }
            }
        }
        ".include" => {
            if let Some(target) = parse_quoted_include_arg(rest) {
                scan_asm_include(&target, db, state, include_depth + 1);
            }
        }
        _ => {}
    }

    let is_gnu_only = db
        .directive(head_raw)
        .is_some_and(|dir| !dir.llvm_supported)
        || matches!(
            head_lower,
            ".eqv"
                | ".func"
                | ".endfunc"
                | ".float16"
                | ".bfloat16"
                | ".dtprelword"
                | ".linkonce"
                | ".stabs"
                | ".list"
                | ".nolist"
                | ".title"
                | ".sbttl"
                | ".psize"
                | ".mri"
        );
    if is_gnu_only {
        out.push(make_diag(
            head_range,
            DiagnosticSeverity::WARNING,
            codes::GNU_ONLY_DIRECTIVE,
            format!("directive `{head_raw}` is GNU as only and not accepted by LLVM"),
            Vec::new(),
        ));
    } else if db.directive(head_raw).is_none()
        && !is_known_extra_directive(head_lower)
        && !state.known_macros.contains(head_lower)
        && !state.has_any_unresolved_include()
    {
        out.push(make_diag(
            head_range,
            DiagnosticSeverity::ERROR,
            codes::UNKNOWN_DIRECTIVE,
            format!("unknown directive `{head_raw}`"),
            Vec::new(),
        ));
    }
}

fn is_known_extra_directive(head_lower: &str) -> bool {
    head_lower.starts_with(".cfi_")
        || head_lower.starts_with(".dc.")
        || head_lower.starts_with(".dcb.")
        || head_lower.starts_with(".ds.")
        || head_lower.starts_with(".cv_")
        || matches!(
            head_lower,
            ".insn"
                | ".addrsig"
                | ".addrsig_sym"
                | ".cg_profile"
                | ".eh_frame"
                | ".lto_discard"
                | ".variant_pcs"
                | ".ln"
                | ".stabs"
                | ".stabd"
                | ".stabn"
                | ".fail"
                | ".offset"
                | ".struct"
                | ".init"
                | ".fini"
                | ".preinit_array"
                | ".init_array"
                | ".fini_array"
                | ".sdata"
                | ".sbss"
                | ".tdata"
                | ".tbss"
                | ".rodata"
                | ".nopic"
                | ".pic"
                | ".relax"
                | ".norelax"
        )
}

fn scan_asm_include(target: &str, db: &Database, state: &mut ValidatorState<'_>, depth: usize) {
    if depth > MAX_ASM_INCLUDE_DEPTH {
        state.has_unresolved_asm_include = true;
        return;
    }
    let Some((resolved, content)) = resolve_asm_include(target, state.current_file, state.file_ctx)
    else {
        state.has_unresolved_asm_include = true;
        return;
    };
    let key = path_key(&resolved);
    if !state.active_includes.insert(key.clone()) {
        return;
    }

    let mut in_block = false;
    let mut local_macro_depth = 0usize;
    for raw_line in content.lines() {
        if matches!(
            classify_hash_line(raw_line),
            Some(HashLineKind::Directive { .. })
        ) {
            continue;
        }
        let stripped = crate::line::strip_comments_with_state(raw_line, &mut in_block, false);
        for raw_stmt in crate::line::split_unquoted_statements_stripped(&stripped) {
            let t = raw_stmt.trim();
            if t.is_empty() {
                continue;
            }
            let Some((head, rest)) = split_head_and_rest(t) else {
                continue;
            };
            let hl = head.to_ascii_lowercase();
            match hl.as_str() {
                ".macro" => {
                    if local_macro_depth == 0
                        && let Some(name) = rest
                            .trim_start()
                            .split(|c: char| c.is_whitespace() || c == ',' || c == '(')
                            .next()
                            .filter(|s| !s.is_empty())
                    {
                        state.known_macros.insert(name.to_ascii_lowercase());
                    }
                    local_macro_depth += 1;
                }
                ".irp" | ".irpc" | ".rept" => local_macro_depth += 1,
                ".endm" | ".endr" => local_macro_depth = local_macro_depth.saturating_sub(1),
                ".equ" | ".equiv" | ".set" if local_macro_depth == 0 => {
                    if let Some((sym, expr)) = rest.split_once(',') {
                        let s = sym.trim();
                        if !s.is_empty() {
                            let val = eval_asm_const_expr(expr.trim(), &state.equ_consts);
                            state.equ_consts.insert(s.to_owned(), val);
                        }
                    }
                }
                ".attribute" if local_macro_depth == 0 => {
                    if let Some(arch) = crate::context::parse_attribute_arch(rest) {
                        state.march.apply_option_arch(arch);
                    }
                }
                ".include" if local_macro_depth == 0 => {
                    if let Some(sub) = parse_quoted_include_arg(rest) {
                        scan_asm_include(&sub, db, state, depth + 1);
                    }
                }
                _ => {}
            }
        }
    }
    let _ = db;
    state.active_includes.remove(&key);
}

fn resolve_asm_include(
    target: &str,
    current_file: Option<&Path>,
    file_ctx: &FileContext,
) -> Option<(PathBuf, String)> {
    let rel = Path::new(target);
    if rel.is_absolute() {
        return read_file_capped(rel).map(|c| (rel.to_path_buf(), c));
    }
    let mut dirs: Vec<&Path> = Vec::new();
    if let Some(dir) = current_file.and_then(Path::parent) {
        dirs.push(dir);
    }
    for d in &file_ctx.quote_dirs {
        dirs.push(d.as_path());
    }
    for d in &file_ctx.include_dirs {
        dirs.push(d.as_path());
    }
    if let Some(wd) = file_ctx.working_dir.as_deref() {
        dirs.push(wd);
    }
    for dir in dirs {
        let cand = dir.join(rel);
        if let Some(c) = read_file_capped(&cand) {
            return Some((cand, c));
        }
    }
    None
}

fn read_file_capped(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_ASM_INCLUDE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn parse_quoted_include_arg(rest: &str) -> Option<String> {
    let inner = rest.trim_start().strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_owned())
}

fn apply_option_directive(rest: &str, state: &mut ValidatorState<'_>) {
    let arg = rest.trim();
    let lower = arg.to_ascii_lowercase();
    if lower == "push" {
        state.option_stack.push(state.march.clone());
    } else if lower == "pop" {
        if let Some(prev) = state.option_stack.pop() {
            state.march = prev;
        }
    } else if lower == "rvc" {
        state.march.apply_rvc(true);
    } else if lower == "norvc" {
        state.march.apply_rvc(false);
    } else if let Some(arch_arg) = lower.strip_prefix("arch")
        && let Some(arch_val) = arch_arg.trim_start().strip_prefix(',')
    {
        state.march.apply_option_arch(arch_val);
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_instruction_statement(
    head_lower: &str,
    head_raw: &str,
    rest: &str,
    stmt_abs_start: usize,
    head_len: usize,
    head_range: Range,
    stmt_range: Range,
    pl: &PreprocessedLine,
    source_text: &str,
    line_index: &LineIndex,
    db: &Database,
    state: &ValidatorState<'_>,
    out: &mut Vec<StoredDiagnostic>,
) {
    if state.known_macros.contains(head_lower) {
        return;
    }

    let Some(inst) = db.instruction(head_lower) else {
        if !state.has_any_unresolved_include() {
            let suggestions = suggest_mnemonics(head_lower, db, &state.march);
            let msg = if suggestions.is_empty() {
                format!("unrecognized instruction mnemonic `{head_raw}`")
            } else {
                format!(
                    "unrecognized instruction mnemonic `{head_raw}`, did you mean: {}?",
                    suggestions.join(", ")
                )
            };
            out.push(make_diag(
                head_range,
                DiagnosticSeverity::ERROR,
                codes::UNKNOWN_MNEMONIC,
                msg,
                suggestions,
            ));
        }
        return;
    };

    if !inst.llvm_supported {
        return;
    }

    let enabled_variants: Vec<&InstructionVariant> = inst
        .variants
        .iter()
        .filter(|v| {
            state
                .march
                .supports_extensions(&v.required_extensions, v.xlen)
        })
        .collect();

    if enabled_variants.is_empty() {
        let any_xlen_match = inst
            .variants
            .iter()
            .any(|v| state.march.supports_xlen(v.xlen));
        if any_xlen_match {
            let mut reqs: Vec<String> = Vec::new();
            for v in inst
                .variants
                .iter()
                .filter(|v| state.march.supports_xlen(v.xlen) && !v.required_extensions.is_empty())
            {
                for req in &v.required_extensions {
                    for alt in req.split('|') {
                        let clean = alt.trim().to_ascii_lowercase();
                        if !clean.is_empty() && !reqs.contains(&clean) {
                            reqs.push(clean);
                        }
                    }
                }
            }
            let req_text = if reqs.is_empty() {
                "a disabled extension".to_owned()
            } else {
                reqs.join(" / ")
            };
            out.push(make_diag(
                stmt_range,
                DiagnosticSeverity::ERROR,
                codes::EXTENSION_DISABLED,
                format!("instruction `{head_raw}` requires extension `{req_text}`"),
                Vec::new(),
            ));
        } else {
            let xlen_label = match state.march.xlen {
                Xlen::Rv32 => "RV32",
                Xlen::Rv64 => "RV64",
                Xlen::Both => "this target",
            };
            out.push(make_diag(
                stmt_range,
                DiagnosticSeverity::ERROR,
                codes::XLEN_MISMATCH,
                format!("instruction `{head_raw}` is not supported on {xlen_label}"),
                Vec::new(),
            ));
        }
        return;
    }

    // Parse comma-separated operands after mnemonic, preserving spans in `pl.text`.
    let rest_abs_base = stmt_abs_start + head_len;
    let Some(user_ops) = split_operands_with_spans(rest, rest_abs_base) else {
        return;
    };

    let mut best_mismatch: Option<(usize, OperandMismatch)> = None;
    let mut any_count_match = false;

    for v in &enabled_variants {
        match match_variant(v, &user_ops, state, db) {
            VariantMatch::ValidOrSkip => return,
            VariantMatch::CountMismatch => {}
            VariantMatch::OperandError { matched, first_err } => {
                any_count_match = true;
                match &best_mismatch {
                    None => best_mismatch = Some((matched, first_err)),
                    Some((best_n, _)) if matched > *best_n => {
                        best_mismatch = Some((matched, first_err));
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some((_, err)) = best_mismatch {
        let range = map_expanded_range(source_text, line_index, pl, err.abs_start, err.abs_end);
        out.push(make_diag(
            range,
            DiagnosticSeverity::ERROR,
            err.code,
            err.message,
            Vec::new(),
        ));
    } else if !any_count_match {
        out.push(make_diag(
            stmt_range,
            DiagnosticSeverity::ERROR,
            codes::OPERAND_COUNT,
            format!("invalid operand count for `{head_raw}`"),
            Vec::new(),
        ));
    }
}

#[derive(Debug, Clone)]
struct UserOperand<'a> {
    text: &'a str,
    abs_start: usize,
    abs_end: usize,
}

#[derive(Debug, Clone)]
enum SyntaxSlot {
    Single {
        op_idx: usize,
        optional: bool,
        variadic_tail: bool,
    },
    ParenReg {
        op_idx: usize,
        allow_zero_offset: bool,
    },
    Mem {
        offset_idx: usize,
        base_idx: usize,
    },
}

enum VariantMatch {
    ValidOrSkip,
    CountMismatch,
    OperandError {
        matched: usize,
        first_err: OperandMismatch,
    },
}

#[derive(Debug, Clone)]
struct OperandMismatch {
    code: &'static str,
    message: String,
    abs_start: usize,
    abs_end: usize,
}

enum OperandVerdict {
    Match,
    Skip,
    Mismatch(OperandMismatch),
}

fn parse_syntax_slots(variant: &InstructionVariant) -> Option<Vec<SyntaxSlot>> {
    let (_mnem, rest) = match split_head_and_rest(variant.syntax.trim()) {
        Some((m, r)) => (m, r),
        None => return Some(Vec::new()),
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(Vec::new());
    }

    let op_index_by_name: HashMap<&str, usize> = variant
        .operands
        .iter()
        .enumerate()
        .map(|(i, o)| (o.name.as_str(), i))
        .collect();

    let raw_slots = split_operands_with_spans(rest, 0)?;
    let mut slots = Vec::with_capacity(raw_slots.len());

    for uo in raw_slots {
        let s = uo.text.trim();
        if let Some((offset_raw, base_inner)) = split_trailing_paren(s) {
            let base_name = base_inner.trim();
            let base_idx = *op_index_by_name.get(base_name)?;
            if offset_raw.trim().is_empty() {
                let allow_zero_offset = variant
                    .operands
                    .get(base_idx)
                    .is_some_and(|op| op.class == "GPRMemZeroOffset");
                slots.push(SyntaxSlot::ParenReg {
                    op_idx: base_idx,
                    allow_zero_offset,
                });
            } else {
                let offset_idx = *op_index_by_name.get(offset_raw.trim())?;
                slots.push(SyntaxSlot::Mem {
                    offset_idx,
                    base_idx,
                });
            }
        } else {
            let op_idx = *op_index_by_name.get(s)?;
            let class = variant.operands.get(op_idx)?.class.as_str();
            let optional = matches!(class, "VMaskOp" | "frmarg" | "frmarglegacy");
            let variadic_tail = matches!(class, "VTypeIOp10" | "VTypeIOp11");
            slots.push(SyntaxSlot::Single {
                op_idx,
                optional,
                variadic_tail,
            });
        }
    }

    Some(slots)
}

fn match_variant(
    variant: &InstructionVariant,
    user_ops: &[UserOperand<'_>],
    state: &ValidatorState<'_>,
    db: &Database,
) -> VariantMatch {
    let Some(slots) = parse_syntax_slots(variant) else {
        return VariantMatch::ValidOrSkip;
    };

    let min_ops = slots
        .iter()
        .filter(|s| !matches!(s, SyntaxSlot::Single { optional: true, .. }))
        .count();
    let has_variadic = slots.iter().any(|s| {
        matches!(
            s,
            SyntaxSlot::Single {
                variadic_tail: true,
                ..
            }
        )
    });
    let max_ops = if has_variadic {
        usize::MAX
    } else {
        slots.len()
    };

    if user_ops.len() < min_ops || user_ops.len() > max_ops {
        return VariantMatch::CountMismatch;
    }

    let mut matched = 0usize;
    let mut first_err: Option<OperandMismatch> = None;

    for (slot_idx, slot) in slots.iter().enumerate() {
        let Some(uo) = user_ops.get(slot_idx) else {
            if matches!(slot, SyntaxSlot::Single { optional: true, .. }) {
                continue;
            }
            return VariantMatch::CountMismatch;
        };

        let verdict = match *slot {
            SyntaxSlot::Single {
                op_idx,
                variadic_tail,
                ..
            } => {
                if variadic_tail {
                    // Variadic tail (e.g. `vsetvli` `vtypei` like `e8, m1, ta, ma`) always succeeds
                    // so multi-token vtype forms are never falsely rejected.
                    OperandVerdict::Skip
                } else if let Some(op) = variant.operands.get(op_idx) {
                    check_operand_class(&variant.syntax, &op.class, uo, state, db)
                } else {
                    OperandVerdict::Skip
                }
            }
            SyntaxSlot::ParenReg {
                op_idx,
                allow_zero_offset,
            } => check_paren_reg_operand(
                variant.operands.get(op_idx).map(|o| o.class.as_str()),
                allow_zero_offset,
                uo,
                state,
                db,
            ),
            SyntaxSlot::Mem {
                offset_idx,
                base_idx,
            } => check_mem_operand(
                variant.operands.get(offset_idx).map(|o| o.class.as_str()),
                variant.operands.get(base_idx).map(|o| o.class.as_str()),
                uo,
                state,
                db,
            ),
        };

        match verdict {
            OperandVerdict::Match | OperandVerdict::Skip => matched += 1,
            OperandVerdict::Mismatch(m) => {
                if first_err.is_none() {
                    first_err = Some(m);
                }
            }
        }
    }

    match first_err {
        None => VariantMatch::ValidOrSkip,
        Some(err) => VariantMatch::OperandError {
            matched,
            first_err: err,
        },
    }
}

fn check_paren_reg_operand(
    base_class: Option<&str>,
    allow_zero_offset: bool,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    let Some(class) = base_class else {
        return OperandVerdict::Skip;
    };
    let Some((off_raw, inner)) = split_trailing_paren(uo.text.trim()) else {
        let trimmed = uo.text.trim();
        if looks_like_pure_register(trimmed, db) {
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_REGISTER,
                message: format!(
                    "expected memory operand `({})`, got register `{trimmed}`",
                    describe_register_class_for_diag(class)
                ),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        return OperandVerdict::Skip;
    };
    let off_trimmed = off_raw.trim();
    if !off_trimmed.is_empty() {
        if !allow_zero_offset {
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!(
                    "expected zero offset for `{}` memory operand",
                    describe_register_class_for_diag(class)
                ),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        match eval_asm_const_expr(off_trimmed, &state.equ_consts) {
            Some(0) => {}
            Some(v) => {
                return OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("memory offset must be 0 here, got {v}"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                });
            }
            None => return OperandVerdict::Skip,
        }
    }
    let reg_uo = UserOperand {
        text: inner.trim(),
        abs_start: uo.abs_start,
        abs_end: uo.abs_end,
    };
    check_operand_class("", class, &reg_uo, state, db)
}

fn check_mem_operand(
    offset_class: Option<&str>,
    base_class: Option<&str>,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    let trimmed = uo.text.trim();
    let Some((off_raw, inner)) = split_trailing_paren(trimmed) else {
        // E.g. `lw a0, sym` where bare `ld rd, imm(rs1)` shouldn't reject when another overload applies,
        // or invalid syntax `lw a0, 0` without `(rs)`.
        return if looks_like_pure_register(trimmed, db) {
            OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_REGISTER,
                message: format!("expected memory operand `offset(base)`, got `{trimmed}`"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            })
        } else {
            OperandVerdict::Skip
        };
    };

    if let Some(bc) = base_class {
        let base_uo = UserOperand {
            text: inner.trim(),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        };
        if let OperandVerdict::Mismatch(m) = check_operand_class("", bc, &base_uo, state, db) {
            return OperandVerdict::Mismatch(m);
        }
    }

    if let Some(oc) = offset_class {
        let off_trimmed = off_raw.trim();
        if off_trimmed.is_empty() {
            return OperandVerdict::Skip;
        }
        let off_uo = UserOperand {
            text: off_trimmed,
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        };
        return check_operand_class("", oc, &off_uo, state, db);
    }

    OperandVerdict::Match
}

fn check_operand_class(
    variant_syntax: &str,
    class: &str,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    let tok = uo.text.trim();
    if tok.is_empty() {
        return OperandVerdict::Skip;
    }

    match class {
        "VMaskOp" => {
            return if tok.eq_ignore_ascii_case("v0.t") {
                OperandVerdict::Match
            } else {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_REGISTER,
                    message: format!("expected vector mask `v0.t`, got `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            };
        }
        "VMaskCarryInOp" => {
            return if tok.eq_ignore_ascii_case("v0") {
                OperandVerdict::Match
            } else {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_REGISTER,
                    message: format!("expected carry-in vector mask `v0`, got `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            };
        }
        "frmarg" | "frmarglegacy" => {
            let lower = tok.to_ascii_lowercase();
            return if matches!(
                lower.as_str(),
                "rne" | "rtz" | "rdn" | "rup" | "rmm" | "dyn"
            ) {
                OperandVerdict::Match
            } else {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("invalid rounding mode `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            };
        }
        "rtzarg" => {
            return if tok.eq_ignore_ascii_case("rtz") {
                OperandVerdict::Match
            } else {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("expected rounding mode `rtz`, got `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            };
        }
        "fencearg" => {
            let valid = !tok.is_empty()
                && tok
                    .bytes()
                    .all(|b| matches!(b.to_ascii_lowercase(), b'i' | b'o' | b'r' | b'w'));
            return if valid {
                OperandVerdict::Match
            } else {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("invalid fence ordering `{tok}` (expected subset of `iorw`)"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            };
        }
        "csr_sysreg" => return check_csr_operand(tok, uo, state, db),
        "bare_symbol"
        | "call_symbol"
        | "pseudo_jump_symbol"
        | "tlsdesc_call_symbol"
        | "tprel_add_symbol" => {
            if let Some((_, inner)) = split_trailing_paren(tok)
                && db.register(inner.trim()).is_some()
            {
                return OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("expected symbol operand, got memory operand `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                });
            }
            return if looks_like_pure_register(tok, db) {
                OperandVerdict::Mismatch(OperandMismatch {
                    code: codes::INVALID_IMMEDIATE,
                    message: format!("expected symbol operand, got register `{tok}`"),
                    abs_start: uo.abs_start,
                    abs_end: uo.abs_end,
                })
            } else {
                OperandVerdict::Skip
            };
        }
        "rlist" | "reglist" | "loadfpimm" | "VScaleOp" | "VTypeIOp10" | "VTypeIOp11" => {
            return OperandVerdict::Skip;
        }
        _ => {}
    }

    if is_register_class(class) {
        return check_register_operand(variant_syntax, class, tok, uo, state, db);
    }

    check_immediate_operand(class, tok, uo, state, db)
}

fn is_register_class(class: &str) -> bool {
    matches!(
        class,
        "GPR"
            | "GPRC"
            | "GPRMem"
            | "GPRCMem"
            | "BasePtr"
            | "BasePtrC"
            | "GPRMemZeroOffset"
            | "GPRNoX0"
            | "GPRNoX2"
            | "GPRNoX0X2"
            | "GPRPairRV32"
            | "GPRPairCRV32"
            | "GPRPairNoX0RV32"
            | "GPRPairRV64"
            | "GPRX1"
            | "GPRX1X5"
            | "GPRX5"
            | "SP"
            | "SPMem"
            | "SR07"
            | "YGPR"
            | "FPR16"
            | "FPR16INX"
            | "FPR32"
            | "FPR32C"
            | "FPR32INX"
            | "FPR64"
            | "FPR64C"
            | "FPR64IN32X"
            | "FPR64INX"
            | "FPR128"
            | "VR"
            | "VRNoV0"
            | "VRM2"
            | "VRM4"
            | "VRM8"
    )
}

fn check_register_operand(
    variant_syntax: &str,
    class: &str,
    tok: &str,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    // Allow unexpanded macro params or unknown symbols when unresolved includes exist.
    let Some(reg) = db.register(tok) else {
        if state.has_any_unresolved_include() {
            return OperandVerdict::Skip;
        }
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_REGISTER,
            message: format!(
                "invalid {}, got `{tok}`",
                describe_register_class_for_diag(class)
            ),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    };

    if reg.class == RegisterClass::Integer
        && reg.number > 15
        && state.march.extensions.contains("e")
    {
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_REGISTER,
            message: format!("register `{tok}` is unavailable under E extension"),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }

    let ok = match class {
        "GPR" | "GPRMem" | "BasePtr" | "GPRMemZeroOffset" | "FPR16INX" | "FPR32INX"
        | "FPR64INX" | "YGPR" => reg.class == RegisterClass::Integer,
        "GPRC" | "GPRCMem" | "BasePtrC" => {
            reg.class == RegisterClass::Integer && (8..=15).contains(&reg.number)
        }
        "GPRNoX0" => {
            reg.class == RegisterClass::Integer
                && (reg.number != 0 || variant_syntax.starts_with("c.addi "))
        }
        "GPRNoX2" => reg.class == RegisterClass::Integer && reg.number != 2,
        "GPRNoX0X2" => reg.class == RegisterClass::Integer && reg.number != 0 && reg.number != 2,
        "GPRX1" => reg.class == RegisterClass::Integer && reg.number == 1,
        "GPRX5" => reg.class == RegisterClass::Integer && reg.number == 5,
        "GPRX1X5" => reg.class == RegisterClass::Integer && (reg.number == 1 || reg.number == 5),
        "SP" | "SPMem" => reg.class == RegisterClass::Integer && reg.number == 2,
        "SR07" => {
            reg.class == RegisterClass::Integer
                && matches!(reg.number, 8 | 9 | 18 | 19 | 20 | 21 | 22 | 23)
        }
        "GPRPairRV32" | "GPRPairRV64" | "FPR64IN32X" => {
            reg.class == RegisterClass::Integer && reg.number.is_multiple_of(2)
        }
        "GPRPairCRV32" => {
            reg.class == RegisterClass::Integer
                && reg.number.is_multiple_of(2)
                && (8..=15).contains(&reg.number)
        }
        "GPRPairNoX0RV32" => {
            reg.class == RegisterClass::Integer && reg.number.is_multiple_of(2) && reg.number != 0
        }
        "FPR16" | "FPR32" | "FPR64" | "FPR128" => reg.class == RegisterClass::Float,
        "FPR32C" | "FPR64C" => reg.class == RegisterClass::Float && (8..=15).contains(&reg.number),
        "VR" => reg.class == RegisterClass::Vector,
        "VRNoV0" => reg.class == RegisterClass::Vector && reg.number != 0,
        "VRM2" => reg.class == RegisterClass::Vector && reg.number.is_multiple_of(2),
        "VRM4" => reg.class == RegisterClass::Vector && reg.number.is_multiple_of(4),
        "VRM8" => reg.class == RegisterClass::Vector && reg.number.is_multiple_of(8),
        _ => true,
    };

    if ok {
        OperandVerdict::Match
    } else {
        OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_REGISTER,
            message: format!(
                "expected {}, got `{tok}`",
                describe_register_class_for_diag(class)
            ),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        })
    }
}

fn describe_register_class_for_diag(class: &str) -> &'static str {
    match class {
        "GPR" | "GPRMem" | "BasePtr" | "GPRMemZeroOffset" | "FPR16INX" | "FPR32INX"
        | "FPR64INX" | "YGPR" => "integer register",
        "GPRC" | "GPRCMem" | "BasePtrC" => "register x8–x15",
        "GPRNoX0" => "non-zero integer register (not x0)",
        "GPRNoX2" => "integer register (not sp)",
        "GPRNoX0X2" => "integer register (not x0 or sp)",
        "GPRX1" => "register ra (x1)",
        "GPRX5" => "register t0 (x5)",
        "GPRX1X5" => "register ra or t0 (x1 or x5)",
        "SP" | "SPMem" => "stack pointer (sp)",
        "SR07" => "saved register (s0–s7)",
        "GPRPairRV32" | "GPRPairRV64" | "FPR64IN32X" => "even integer register",
        "GPRPairCRV32" => "even integer register in x8–x14",
        "GPRPairNoX0RV32" => "non-zero even integer register",
        "FPR16" | "FPR32" | "FPR64" | "FPR128" => "floating-point register",
        "FPR32C" | "FPR64C" => "floating-point register f8–f15",
        "VR" => "vector register",
        "VRNoV0" => "vector register (not v0)",
        "VRM2" => "even vector register group (LMUL=2)",
        "VRM4" => "4-aligned vector register group (LMUL=4)",
        "VRM8" => "8-aligned vector register group (LMUL=8)",
        _ => "register",
    }
}

fn check_csr_operand(
    tok: &str,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    if let Some(csr) = db.csr(tok) {
        if !csr.llvm_supported {
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_CSR,
                message: format!("CSR `{tok}` is not supported by LLVM"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        if !state.march.supports_xlen(csr.xlen) {
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_CSR,
                message: format!("CSR `{tok}` is not available for current XLEN"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        return OperandVerdict::Match;
    }

    if let Some(val) = eval_asm_const_expr(tok, &state.equ_consts) {
        if (0..=0xfff).contains(&val) {
            return OperandVerdict::Match;
        }
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_CSR,
            message: format!("CSR address {val} out of 12-bit range (0..4095)"),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }

    if state.has_any_unresolved_include() || state.equ_consts.contains_key(tok) {
        return OperandVerdict::Skip;
    }

    OperandVerdict::Mismatch(OperandMismatch {
        code: codes::INVALID_CSR,
        message: format!("unknown CSR `{tok}`"),
        abs_start: uo.abs_start,
        abs_end: uo.abs_end,
    })
}

fn check_immediate_operand(
    class: &str,
    tok: &str,
    uo: &UserOperand<'_>,
    state: &ValidatorState<'_>,
    db: &Database,
) -> OperandVerdict {
    if tok.contains('%') || tok.contains('"') {
        return OperandVerdict::Skip;
    }

    // Reject obvious register tokens passed in immediate slots immediately.
    if looks_like_pure_register(tok, db) {
        let expected = parse_immediate_spec(class, state.march.xlen)
            .map(|s| describe_imm_constraint(&s))
            .unwrap_or_else(|| "immediate operand".to_owned());
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_IMMEDIATE,
            message: format!("expected {expected}, got register `{tok}`"),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }

    let Some(val) = eval_asm_const_expr(tok, &state.equ_consts) else {
        return OperandVerdict::Skip;
    };

    match class {
        "c_lui_imm" => {
            if (1..=31).contains(&val) || (0xfffe0..=0xfffff).contains(&val) {
                return OperandVerdict::Match;
            }
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!(
                    "immediate {val} out of range for `c.lui` (1..31 or 0xfffe0..0xfffff)"
                ),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        "imm5_zibi" => {
            if val == -1 || (1..=31).contains(&val) {
                return OperandVerdict::Match;
            }
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!("immediate {val} must be 1..31 or -1"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        "stackadj" => {
            if val > 0 && val % 16 == 0 && val <= 128 {
                return OperandVerdict::Match;
            }
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!("stack adjustment {val} must be positive and 16-byte aligned"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        "negstackadj" => {
            if val < 0 && (-128..=-16).contains(&val) && val.rem_euclid(16) == 0 {
                return OperandVerdict::Match;
            }
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!("stack adjustment {val} must be negative and 16-byte aligned"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        "ixlenimm_li" | "ixlenimm_li_restricted" => {
            let valid = if state.march.xlen == Xlen::Rv32 {
                (i32::MIN as i128..=u32::MAX as i128).contains(&val)
            } else {
                (i64::MIN as i128..=u64::MAX as i128).contains(&val)
            };
            if valid {
                return OperandVerdict::Match;
            }
            return OperandVerdict::Mismatch(OperandMismatch {
                code: codes::INVALID_IMMEDIATE,
                message: format!("immediate {val} out of XLEN range"),
                abs_start: uo.abs_start,
                abs_end: uo.abs_end,
            });
        }
        _ => {}
    }

    let Some(spec) = parse_immediate_spec(class, state.march.xlen) else {
        return OperandVerdict::Skip;
    };

    let min = i128::from(spec.min);
    let max = i128::from(spec.max);
    let step = i128::from(spec.step);

    if val < min || val > max {
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_IMMEDIATE,
            message: format!(
                "operand out of range, must be {}",
                describe_imm_constraint(&spec)
            ),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }
    if spec.nonzero && val == 0 {
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_IMMEDIATE,
            message: format!("operand must be nonzero {}", describe_imm_constraint(&spec)),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }
    if step > 1 && val.rem_euclid(step) != 0 {
        return OperandVerdict::Mismatch(OperandMismatch {
            code: codes::INVALID_IMMEDIATE,
            message: format!(
                "operand must be a multiple of {}, expected {}",
                spec.step,
                describe_imm_constraint(&spec)
            ),
            abs_start: uo.abs_start,
            abs_end: uo.abs_end,
        });
    }

    OperandVerdict::Match
}

fn describe_imm_constraint(spec: &crate::hover::operands::ImmSpec) -> String {
    if spec.step > 1 {
        format!("multiple of {} in [{}, {}]", spec.step, spec.min, spec.max)
    } else {
        format!("integer in [{}, {}]", spec.min, spec.max)
    }
}

fn looks_like_pure_register(tok: &str, db: &Database) -> bool {
    db.register(tok).is_some()
}

fn split_head_and_rest(stmt: &str) -> Option<(&str, &str)> {
    let trimmed = stmt.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        Some(idx) => Some((&trimmed[..idx], trimmed[idx..].trim_start())),
        None => Some((trimmed, "")),
    }
}

fn split_statements_with_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
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
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b';' => {
                    out.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push((start, bytes.len()));
    }
    out
}

fn strip_leading_labels_rel(stmt: &str) -> (usize, usize) {
    let mut cur = 0usize;
    loop {
        let rest = &stmt[cur..];
        let trimmed = rest.trim_start();
        cur += rest.len() - trimmed.len();
        if trimmed.is_empty() {
            break;
        }
        if let Some(colon) = find_label_colon(trimmed) {
            cur += colon + 1;
        } else {
            break;
        }
    }
    let end = stmt[cur..].trim_end().len() + cur;
    (cur, end)
}

fn find_label_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$') {
            i += 1;
        } else if b == b':' && i > 0 {
            return Some(i);
        } else {
            return None;
        }
    }
    None
}

fn is_symbol_assignment(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'$'))
    {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    let after = trimmed[i..].trim_start();
    after.starts_with('=') && !after.starts_with("==")
}

fn update_assignment_const(trimmed: &str, equ_consts: &mut HashMap<String, Option<i128>>) {
    let Some((lhs, rhs)) = trimmed.split_once('=') else {
        return;
    };
    let name = lhs.trim();
    if name == "." || name.is_empty() {
        return;
    }
    let val = eval_asm_const_expr(rhs.trim(), equ_consts);
    equ_consts.insert(name.to_owned(), val);
}

fn split_operands_with_spans<'a>(
    rest: &'a str,
    base_offset: usize,
) -> Option<Vec<UserOperand<'a>>> {
    let trimmed_start = rest.len() - rest.trim_start().len();
    let trimmed = rest[trimmed_start..].trim_end();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }

    let bytes = trimmed.as_bytes();
    let mut out = Vec::new();
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut op_start = 0usize;
    let mut i = 0usize;

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
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'(' => paren_depth += 1,
                b')' => {
                    if paren_depth == 0 {
                        return None;
                    }
                    paren_depth -= 1;
                }
                b'{' => brace_depth += 1,
                b'}' => {
                    if brace_depth == 0 {
                        return None;
                    }
                    brace_depth -= 1;
                }
                b',' if paren_depth == 0 && brace_depth == 0 => {
                    push_trimmed_op(trimmed, op_start, i, base_offset + trimmed_start, &mut out);
                    op_start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }

    if in_single || in_double || paren_depth != 0 || brace_depth != 0 {
        return None;
    }

    push_trimmed_op(
        trimmed,
        op_start,
        bytes.len(),
        base_offset + trimmed_start,
        &mut out,
    );
    Some(out)
}

fn push_trimmed_op<'a>(
    buf: &'a str,
    start: usize,
    end: usize,
    base: usize,
    out: &mut Vec<UserOperand<'a>>,
) {
    let raw = &buf[start..end];
    let lead = raw.len() - raw.trim_start().len();
    let core = raw[lead..].trim_end();
    let s = base + start + lead;
    let e = s + core.len();
    out.push(UserOperand {
        text: core,
        abs_start: s,
        abs_end: e,
    });
}

fn split_trailing_paren(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim();
    if !trimmed.ends_with(')') {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut depth = 0usize;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    let outer = &trimmed[..i];
                    let inner = &trimmed[i + 1..bytes.len() - 1];
                    return Some((outer, inner));
                }
            }
            _ => {}
        }
    }
    None
}

fn eval_asm_const_expr(expr: &str, equs: &HashMap<String, Option<i128>>) -> Option<i128> {
    let toks = tokenize_asm_expr(expr, equs)?;
    if toks.is_empty() {
        return None;
    }
    let mut p = AsmExprParser {
        toks: &toks,
        pos: 0,
        depth: 0,
    };
    let v = p.parse_or()?;
    if p.pos != toks.len() {
        return None;
    }
    Some(v)
}

#[derive(Debug, Clone, Copy)]
enum AsmTok {
    Int(i128),
    Plus,
    Minus,
    BitNot,
    LogNot,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    BitAnd,
    BitXor,
    BitOr,
    LParen,
    RParen,
}

fn tokenize_asm_expr(s: &str, equs: &HashMap<String, Option<i128>>) -> Option<Vec<AsmTok>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'\'' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else if j < bytes.len() {
                j += 1;
            } else {
                return None;
            }
            if bytes.get(j) != Some(&b'\'') {
                return None;
            }
            let ch_val = if bytes[i + 1] == b'\\' {
                match bytes[i + 2] {
                    b'n' => b'\n' as i128,
                    b'r' => b'\r' as i128,
                    b't' => b'\t' as i128,
                    b'0' => 0,
                    other => other as i128,
                }
            } else {
                bytes[i + 1] as i128
            };
            out.push(AsmTok::Int(ch_val));
            i = j + 1;
            continue;
        }
        if b.is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let raw = &s[i..j];
            let val = parse_int_literal(raw)?;
            out.push(AsmTok::Int(val));
            i = j;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let ident = &s[i..j];
            let val = equs.get(ident).copied().flatten()?;
            out.push(AsmTok::Int(val));
            i = j;
            continue;
        }
        if b == b'<' && bytes.get(i + 1) == Some(&b'<') {
            out.push(AsmTok::Shl);
            i += 2;
            continue;
        }
        if b == b'>' && bytes.get(i + 1) == Some(&b'>') {
            out.push(AsmTok::Shr);
            i += 2;
            continue;
        }
        let tok = match b {
            b'+' => AsmTok::Plus,
            b'-' => AsmTok::Minus,
            b'~' => AsmTok::BitNot,
            b'!' => AsmTok::LogNot,
            b'*' => AsmTok::Mul,
            b'/' => AsmTok::Div,
            b'%' => AsmTok::Rem,
            b'&' => AsmTok::BitAnd,
            b'^' => AsmTok::BitXor,
            b'|' => AsmTok::BitOr,
            b'(' => AsmTok::LParen,
            b')' => AsmTok::RParen,
            _ => return None,
        };
        out.push(tok);
        i += 1;
    }

    Some(out)
}

fn parse_int_literal(raw: &str) -> Option<i128> {
    let clean = raw.trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = clean
        .strip_prefix("0x")
        .or_else(|| clean.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = clean
        .strip_prefix("0b")
        .or_else(|| clean.strip_prefix("0B"))
    {
        i128::from_str_radix(bin, 2).ok()
    } else if clean.starts_with('0') && clean.len() > 1 {
        i128::from_str_radix(&clean[1..], 8).ok()
    } else {
        clean.parse::<i128>().ok()
    }
}

struct AsmExprParser<'a> {
    toks: &'a [AsmTok],
    pos: usize,
    depth: usize,
}

impl<'a> AsmExprParser<'a> {
    fn peek(&self) -> Option<AsmTok> {
        self.toks.get(self.pos).copied()
    }

    fn parse_or(&mut self) -> Option<i128> {
        let mut lhs = self.parse_xor()?;
        while matches!(self.peek(), Some(AsmTok::BitOr)) {
            self.pos += 1;
            lhs |= self.parse_xor()?;
        }
        Some(lhs)
    }

    fn parse_xor(&mut self) -> Option<i128> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(AsmTok::BitXor)) {
            self.pos += 1;
            lhs ^= self.parse_and()?;
        }
        Some(lhs)
    }

    fn parse_and(&mut self) -> Option<i128> {
        let mut lhs = self.parse_shift()?;
        while matches!(self.peek(), Some(AsmTok::BitAnd)) {
            self.pos += 1;
            lhs &= self.parse_shift()?;
        }
        Some(lhs)
    }

    fn parse_shift(&mut self) -> Option<i128> {
        let mut lhs = self.parse_add()?;
        loop {
            match self.peek() {
                Some(AsmTok::Shl) => {
                    self.pos += 1;
                    let rhs = u32::try_from(self.parse_add()?).ok()?;
                    lhs = lhs.wrapping_shl(rhs);
                }
                Some(AsmTok::Shr) => {
                    self.pos += 1;
                    let rhs = u32::try_from(self.parse_add()?).ok()?;
                    lhs = lhs.wrapping_shr(rhs);
                }
                _ => break,
            }
        }
        Some(lhs)
    }

    fn parse_add(&mut self) -> Option<i128> {
        let mut lhs = self.parse_mul()?;
        loop {
            match self.peek() {
                Some(AsmTok::Plus) => {
                    self.pos += 1;
                    lhs = lhs.wrapping_add(self.parse_mul()?);
                }
                Some(AsmTok::Minus) => {
                    self.pos += 1;
                    lhs = lhs.wrapping_sub(self.parse_mul()?);
                }
                _ => break,
            }
        }
        Some(lhs)
    }

    fn parse_mul(&mut self) -> Option<i128> {
        let mut lhs = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(AsmTok::Mul) => {
                    self.pos += 1;
                    lhs = lhs.wrapping_mul(self.parse_unary()?);
                }
                Some(AsmTok::Div) => {
                    self.pos += 1;
                    lhs = lhs.checked_div(self.parse_unary()?)?;
                }
                Some(AsmTok::Rem) => {
                    self.pos += 1;
                    lhs = lhs.checked_rem(self.parse_unary()?)?;
                }
                _ => break,
            }
        }
        Some(lhs)
    }

    fn parse_unary(&mut self) -> Option<i128> {
        if self.depth >= 64 {
            return None;
        }
        self.depth += 1;
        let res = match self.peek() {
            Some(AsmTok::Plus) => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(AsmTok::Minus) => {
                self.pos += 1;
                Some(self.parse_unary()?.wrapping_neg())
            }
            Some(AsmTok::BitNot) => {
                self.pos += 1;
                Some(!self.parse_unary()?)
            }
            Some(AsmTok::LogNot) => {
                self.pos += 1;
                Some(i128::from(self.parse_unary()? == 0))
            }
            _ => self.parse_primary(),
        };
        self.depth -= 1;
        res
    }

    fn parse_primary(&mut self) -> Option<i128> {
        match self.peek()? {
            AsmTok::LParen => {
                self.pos += 1;
                let v = self.parse_or()?;
                if !matches!(self.peek(), Some(AsmTok::RParen)) {
                    return None;
                }
                self.pos += 1;
                Some(v)
            }
            AsmTok::Int(v) => {
                self.pos += 1;
                Some(v)
            }
            _ => None,
        }
    }
}

fn suggest_mnemonics(query: &str, db: &Database, march: &MarchSpec) -> Vec<String> {
    let mut best: Vec<(usize, &str)> = Vec::new();
    for inst in &db.raw.instructions {
        if !inst.llvm_supported {
            continue;
        }
        if !inst
            .variants
            .iter()
            .any(|v| march.supports_extensions(&v.required_extensions, v.xlen))
        {
            continue;
        }
        let dist = levenshtein(query, &inst.mnemonic);
        if dist <= 2 {
            best.push((dist, &inst.mnemonic));
        }
    }
    best.sort_by_key(|(d, m)| (*d, *m));
    best.into_iter()
        .take(3)
        .map(|(_, m)| m.to_owned())
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len().abs_diff(b_bytes.len()) > 2 {
        return 3;
    }
    let mut prev: Vec<usize> = (0..=b_bytes.len()).collect();
    let mut curr = vec![0; b_bytes.len() + 1];
    for (i, &ac) in a_bytes.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b_bytes.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_bytes.len()]
}

fn map_expanded_range(
    source_text: &str,
    line_index: &LineIndex,
    pl: &PreprocessedLine,
    exp_start: usize,
    exp_end: usize,
) -> Range {
    let overlapping: Vec<_> = pl
        .spans
        .iter()
        .filter(|s| s.expanded.1 > exp_start && s.expanded.0 < exp_end)
        .collect();
    if let Some(first) = overlapping.first() {
        let line = first.source_line as u32;
        let start_col = first.source_utf16.0;
        let end_col = overlapping
            .iter()
            .filter(|s| s.source_line == first.source_line)
            .map(|s| s.source_utf16.1)
            .max()
            .unwrap_or(first.source_utf16.1);
        return Range::new(Position::new(line, start_col), Position::new(line, end_col));
    }
    let line = pl.source_line as u32;
    let (s, e, _) = line_index.line_bounds(source_text, pl.source_line);
    let end_col = source_text
        .get(s..e)
        .map_or(0, |slice| slice.encode_utf16().count() as u32);
    Range::new(Position::new(line, 0), Position::new(line, end_col))
}

fn make_diag(
    range: Range,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
    suggestions: Vec<String>,
) -> StoredDiagnostic {
    StoredDiagnostic {
        diagnostic: Diagnostic {
            range,
            severity: Some(severity),
            code: Some(NumberOrString::String(code.to_owned())),
            source: Some(SOURCE_NAME.to_owned()),
            message,
            ..Diagnostic::default()
        },
        suggestions,
        macro_def_hints: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{CompileContext, test_uri};

    fn check(march: &str, src: &str, suppress: &[&str]) -> Vec<StoredDiagnostic> {
        let db = Database::load().expect("db");
        let mut ctx = CompileContext::new(
            None,
            Config {
                default_march: march.to_owned(),
                ..Config::default()
            },
        );
        let uri = test_uri("builtin_unit.S");
        let file_ctx = ctx.for_uri(&uri);
        let sup: Vec<String> = suppress.iter().map(|s| (*s).to_owned()).collect();
        run_builtin_diagnostics(&uri, src, &file_ctx, &db, &sup)
    }

    fn diag_codes(diags: &[StoredDiagnostic]) -> Vec<&str> {
        diags
            .iter()
            .filter_map(|d| match d.diagnostic.code.as_ref()? {
                NumberOrString::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reports_all_eight_codes_accurately_and_supports_suppress() {
        let src = "\
    mvi a0, 1
    sh1add a0, a1, a2
    addi a0, a1
    c.lwsp x0, 0(sp)
    addi a0, a1, 4096
    csrr a0, not_a_csr
    .func foo
";
        let diags = check("rv64gc", src, &[]);
        assert_eq!(
            diag_codes(&diags),
            vec![
                codes::UNKNOWN_MNEMONIC,
                codes::EXTENSION_DISABLED,
                codes::OPERAND_COUNT,
                codes::INVALID_REGISTER,
                codes::INVALID_IMMEDIATE,
                codes::INVALID_CSR,
                codes::GNU_ONLY_DIRECTIVE,
            ]
        );
        assert!(diags[0].suggestions.contains(&"mv".to_owned()));

        let diags32 = check(
            "rv32imac",
            "    addiw a0, a1, 1\n    csrr a0, mcycleh\n",
            &[],
        );
        assert_eq!(diag_codes(&diags32), vec![codes::XLEN_MISMATCH]);

        let diags64_csr = check("rv64gc", "    csrr a0, mcycleh\n", &[]);
        assert_eq!(diag_codes(&diags64_csr), vec![codes::INVALID_CSR]);

        let suppressed = check("rv64gc", src, &[codes::GNU_ONLY_DIRECTIVE]);
        assert!(!diag_codes(&suppressed).contains(&codes::GNU_ONLY_DIRECTIVE));
    }

    #[test]
    fn respects_skip_rules_option_directives_equ_reassignment_and_utf16() {
        let src = "\
#if 0
    bogus_inactive a0
#endif
#include \"missing_header.h\"
#ifdef UNKNOWN_FLAG
    bogus_uncertain a0
#endif
.macro GEN_BAD x
    bogus_in_macro \\x
.endm
.irp reg, a0, a1
    bogus_in_irp \\reg
.endr
.equ OFF, 16
    ld a0, OFF(sp)
.equ OFF, sym_unknown
    addi a0, a1, OFF
    lui a0, %hi(sym)
    /* 日😀 */ mvi a0, 1
";
        let diags = check("rv64gc", src, &[]);
        // Inactive `#if 0` emits only the suppressible `inactive-branch` hint, with 0 errors/warnings:
        assert_eq!(diag_codes(&diags), vec![codes::INACTIVE_BRANCH]);
        assert!(check("rv64gc", src, &[codes::INACTIVE_BRANCH]).is_empty());

        let mb_src = "    /* 日😀 */ addi a0, a1, 5000\n";
        let mb_diags = check("rv64gc", mb_src, &[]);
        assert_eq!(diag_codes(&mb_diags), vec![codes::INVALID_IMMEDIATE]);
        // `    /* 日😀 */ addi a0, a1, ` -> 4 spaces + `/* ` (3) + `日` (1) + `😀` (2) + ` */ ` (4) + `addi a0, a1, ` (13) = 27 UTF-16 code units
        assert_eq!(mb_diags[0].diagnostic.range.start, Position::new(0, 27));
        assert_eq!(mb_diags[0].diagnostic.range.end, Position::new(0, 31));
    }
}
