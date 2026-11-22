use std::collections::BTreeSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind,
    Range,
};

use crate::context::MarchSpec;
use crate::data::{Database, InstructionKind, InstructionVariant, RegisterClass};
use crate::index::{CompletionSymbol, SymKind};

use super::snippets::{
    build_instruction_snippet, deduplicate_and_sort_variants, directive_snippet, syntax_args,
    trailing_operand_sep, variant_slot_count,
};
use super::{CompletionClientCaps, make_item, make_rich_item, match_rank};

pub(super) const OPTION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("arch", "Modify or set active ISA architecture extensions"),
    ("rvc", "Enable compressed (C) instruction generation"),
    ("norvc", "Disable compressed (C) instruction generation"),
    ("push", "Push current .option state onto the option stack"),
    ("pop", "Restore previous .option state from the stack"),
    ("relax", "Enable linker relaxation"),
    ("norelax", "Disable linker relaxation"),
    ("pic", "Generate position-independent code"),
    ("nopic", "Generate position-dependent code"),
    (
        "exact",
        "Assemble instructions exactly as written without relaxation",
    ),
];

const RELOCATION_FUNCTIONS: &[(&str, &str)] = &[
    ("%hi(", "Bits [31:12] of absolute symbol address"),
    ("%lo(", "Bits [11:0] of absolute symbol address"),
    ("%pcrel_hi(", "Bits [31:12] of PC-relative symbol address"),
    (
        "%pcrel_lo(",
        "Bits [11:0] offset referencing the matching %pcrel_hi label",
    ),
    ("%tprel_hi(", "Bits [31:12] of TLS local-exec offset"),
    ("%tprel_lo(", "Bits [11:0] of TLS local-exec offset"),
    ("%tprel_add(", "TLS tp-relative add relocation annotation"),
    (
        "%got_pcrel_hi(",
        "Bits [31:12] of PC-relative GOT entry address",
    ),
];

const ROUNDING_MODES: &[(&str, &str)] = &[
    ("rne", "Round to Nearest, ties to Even"),
    ("rtz", "Round towards Zero"),
    ("rdn", "Round Down (towards -infinity)"),
    ("rup", "Round Up (towards +infinity)"),
    ("rmm", "Round to Nearest, ties to Max Magnitude"),
    ("dyn", "Dynamic rounding mode (use frm CSR)"),
];

const FENCE_ARGS: &[(&str, &str)] = &[
    ("iorw", "Input, Output, Read, Write"),
    ("rw", "Read and Write"),
    ("r", "Read"),
    ("w", "Write"),
    ("i", "Input"),
    ("o", "Output"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateHint {
    IntReg,
    FloatReg,
    VecReg,
    Csr,
    Frm,
    Fence,
    Vmask,
    SymbolOrImm,
}

pub(super) fn collect_instruction_completions(
    db: &Database,
    march: &MarchSpec,
    typed_lower: &str,
    replace_range: Range,
    next_is_ws: bool,
    caps: CompletionClientCaps,
    items: &mut Vec<CompletionItem>,
) {
    for inst in &db.raw.instructions {
        if !inst.llvm_supported {
            continue;
        }
        let matching_variants: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
            .collect();
        if matching_variants.is_empty() {
            continue;
        }

        let kind = match inst.kind {
            InstructionKind::Instruction => CompletionItemKind::FUNCTION,
            InstructionKind::Alias | InstructionKind::Pseudo => CompletionItemKind::SNIPPET,
        };
        let rank = match_rank(&inst.mnemonic, typed_lower);
        let mut unique_variants = deduplicate_and_sort_variants(&inst.mnemonic, &matching_variants);

        if !caps.snippet_support {
            let best = unique_variants.first().copied();
            let has_ops = unique_variants
                .iter()
                .any(|v| variant_slot_count(&inst.mnemonic, &v.syntax) > 0);
            let new_text = if has_ops && !next_is_ws {
                format!("{} ", inst.mnemonic)
            } else {
                inst.mnemonic.clone()
            };
            let detail = best.map(|v| v.syntax.clone());
            let documentation = format_instruction_doc(
                None,
                inst.long_name.as_deref(),
                inst.description.as_deref(),
            );
            let sort = format!("{rank}_1_{}", inst.mnemonic);
            items.push(make_rich_item(
                inst.mnemonic.clone(),
                inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                documentation,
                Some(InsertTextFormat::PLAIN_TEXT),
                sort,
                replace_range,
            ));
            continue;
        }

        for (idx, v) in unique_variants.drain(..).enumerate() {
            let args_str = syntax_args(&inst.mnemonic, &v.syntax);
            let (new_text, format) = build_instruction_snippet(&inst.mnemonic, args_str, v);
            // Put operand syntax in `label` so variants are distinct in Zed (`ld rd, imm12(rs1)`),
            // keep the mnemonic in `filterText`, and keep the long name in `detail` (Item 4c).
            let label = v.syntax.clone();
            let detail = inst.long_name.clone();
            let documentation = format_instruction_doc(
                Some(&v.syntax),
                inst.long_name.as_deref(),
                inst.description.as_deref(),
            );
            let sort = format!("{rank}_1_{}_{idx:02}", inst.mnemonic);

            items.push(make_rich_item(
                label,
                inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                documentation,
                Some(format),
                sort,
                replace_range,
            ));
        }
    }
}

fn format_instruction_doc(
    variant_syntax: Option<&str>,
    long_name: Option<&str>,
    desc: Option<&str>,
) -> Option<Documentation> {
    let mut parts = Vec::new();
    if let Some(syn) = variant_syntax {
        parts.push(format!("```riscv\n{syn}\n```"));
    }
    if let Some(ln) = long_name {
        parts.push(format!("**{ln}**"));
    }
    if let Some(d) = desc.map(first_sentence).filter(|s| !s.is_empty()) {
        parts.push(d);
    }
    if parts.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: parts.join("\n\n"),
    }))
}

fn first_sentence(desc: &str) -> String {
    let para = desc.split("\n\n").next().unwrap_or(desc).trim();
    let mut in_tick = false;
    for (idx, ch) in para.char_indices() {
        if ch == '`' {
            in_tick = !in_tick;
        } else if ch == '.' && !in_tick {
            let next_ok = para[idx + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
            if next_ok {
                return para[..=idx].replace('\n', " ");
            }
        }
    }
    para.replace('\n', " ")
}

pub(super) fn collect_directive_completions(
    db: &Database,
    typed_lower: &str,
    replace_range: Range,
    next_is_ws: bool,
    caps: CompletionClientCaps,
    items: &mut Vec<CompletionItem>,
) {
    for dir in &db.raw.directives {
        if !dir.llvm_supported {
            continue;
        }
        let rank = match_rank(&dir.name, typed_lower);

        if caps.snippet_support && dir.name == ".option" {
            items.push(make_rich_item(
                ".option push ... pop".to_owned(),
                ".option".to_owned(),
                ".option push\n.option ${1:norvc}\n$0\n.option pop".to_owned(),
                CompletionItemKind::SNIPPET,
                None,
                Some(Documentation::String(dir.description.clone())),
                Some(InsertTextFormat::SNIPPET),
                format!("{rank}_2_.option_0"),
                replace_range,
            ));

            let inline_text = if next_is_ws {
                ".option".to_owned()
            } else {
                ".option ".to_owned()
            };
            items.push(make_rich_item(
                ".option <opt>".to_owned(),
                ".option".to_owned(),
                inline_text,
                CompletionItemKind::KEYWORD,
                None,
                Some(Documentation::String(dir.description.clone())),
                Some(InsertTextFormat::PLAIN_TEXT),
                format!("{rank}_2_.option_1"),
                replace_range,
            ));
            continue;
        }

        let (label, new_text, format) = if caps.snippet_support
            && let Some((snippet, hint)) = directive_snippet(&dir.name)
        {
            (
                format!("{} {hint}", dir.name),
                snippet.to_owned(),
                InsertTextFormat::SNIPPET,
            )
        } else {
            let text = if !next_is_ws && dir.arguments.is_some() {
                format!("{} ", dir.name)
            } else {
                dir.name.clone()
            };
            (dir.name.clone(), text, InsertTextFormat::PLAIN_TEXT)
        };

        let sort = format!("{rank}_2_{}", dir.name);
        items.push(make_rich_item(
            label,
            dir.name.clone(),
            new_text,
            CompletionItemKind::KEYWORD,
            dir.arguments.clone(),
            Some(Documentation::String(dir.description.clone())),
            Some(format),
            sort,
            replace_range,
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_operand_completions(
    db: &Database,
    march: &MarchSpec,
    mnemonic: &str,
    operand_index: usize,
    inside_parens: bool,
    next_is_comma: bool,
    visible_syms: &[CompletionSymbol],
    in_macro_body: bool,
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    let mut want_int = false;
    let mut want_float = false;
    let mut want_vec = false;
    let mut want_csr = false;
    let mut want_symbol_or_reloc = false;
    let mut want_frm = false;
    let mut want_fence = false;
    let mut want_vmask = false;
    let mut active_variants: Vec<&InstructionVariant> = Vec::new();

    if inside_parens {
        want_int = true;
        want_symbol_or_reloc = true;
    } else if let Some(inst) = db.instruction(mnemonic) {
        let variants: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
            .collect();
        active_variants = if variants.is_empty() {
            inst.variants.iter().collect()
        } else {
            variants
        };

        let mut matched_any = false;
        for &v in &active_variants {
            if let Some(op) = v.operands.get(operand_index) {
                matched_any = true;
                classify_operand_class(
                    &op.class,
                    &mut want_int,
                    &mut want_float,
                    &mut want_vec,
                    &mut want_csr,
                    &mut want_symbol_or_reloc,
                    &mut want_frm,
                    &mut want_fence,
                    &mut want_vmask,
                );
            }
        }
        if !matched_any && operand_index == 0 {
            want_int = true;
        }
    } else {
        want_symbol_or_reloc = true;
        if !mnemonic.starts_with('.') {
            want_int = true;
        }
    }

    let sep_for = |hint: CandidateHint| {
        trailing_operand_sep(
            &active_variants,
            mnemonic,
            operand_index,
            inside_parens,
            next_is_comma,
            hint,
        )
    };

    if want_int {
        push_register_completions(
            db,
            RegisterClass::Integer,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::IntReg),
            items,
        );
    }
    if want_float {
        push_register_completions(
            db,
            RegisterClass::Float,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::FloatReg),
            items,
        );
    }
    if want_vec {
        push_register_completions(
            db,
            RegisterClass::Vector,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::VecReg),
            items,
        );
    }
    if want_csr {
        push_csr_completions(
            db,
            march,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::Csr),
            items,
        );
    }
    if want_frm {
        let frm_sep = sep_for(CandidateHint::Frm);
        for (name, desc) in ROUNDING_MODES {
            let rank = match_rank(name, typed_lower);
            items.push(make_rich_item(
                (*name).to_owned(),
                (*name).to_owned(),
                format!("{name}{frm_sep}"),
                CompletionItemKind::ENUM_MEMBER,
                Some("rounding mode".to_owned()),
                Some(Documentation::String((*desc).to_owned())),
                None,
                format!("{rank}_0_{name}"),
                replace_range,
            ));
        }
    }
    if want_fence {
        let fence_sep = sep_for(CandidateHint::Fence);
        for (name, desc) in FENCE_ARGS {
            let rank = match_rank(name, typed_lower);
            items.push(make_rich_item(
                (*name).to_owned(),
                (*name).to_owned(),
                format!("{name}{fence_sep}"),
                CompletionItemKind::ENUM_MEMBER,
                Some("fence ordering".to_owned()),
                Some(Documentation::String((*desc).to_owned())),
                None,
                format!("{rank}_0_{name}"),
                replace_range,
            ));
        }
    }
    if want_vmask {
        let rank = match_rank("v0.t", typed_lower);
        let vmask_sep = sep_for(CandidateHint::Vmask);
        items.push(make_rich_item(
            "v0.t".to_owned(),
            "v0.t".to_owned(),
            format!("v0.t{vmask_sep}"),
            CompletionItemKind::VALUE,
            Some("vector mask register".to_owned()),
            None,
            None,
            format!("{rank}_0_v0.t"),
            replace_range,
        ));
    }
    if want_symbol_or_reloc {
        let sym_sep = sep_for(CandidateHint::SymbolOrImm);
        for sym in visible_syms {
            if sym.kind == SymKind::MacroParam {
                continue;
            }
            let kind = match sym.kind {
                SymKind::Equ | SymKind::Define => CompletionItemKind::CONSTANT,
                _ => CompletionItemKind::REFERENCE,
            };
            let rank = match_rank(&sym.name, typed_lower);
            let sort = format!("{rank}_2_{}", sym.name.to_ascii_lowercase());
            items.push(make_rich_item(
                sym.name.clone(),
                sym.name.clone(),
                format!("{}{sym_sep}", sym.name),
                kind,
                sym.detail.clone(),
                None,
                None,
                sort,
                replace_range,
            ));
        }
        push_relocation_completions(typed_lower, replace_range, items);
    }
    if in_macro_body {
        let param_sep = if active_variants.is_empty() {
            ""
        } else {
            sep_for(CandidateHint::IntReg)
        };
        push_macro_param_completions(visible_syms, typed_lower, replace_range, param_sep, items);
    }
}

pub(super) fn operand_class_matches_hint(class: &str, hint: CandidateHint) -> bool {
    let mut int = false;
    let mut float = false;
    let mut vec = false;
    let mut csr = false;
    let mut sym = false;
    let mut frm = false;
    let mut fence = false;
    let mut vmask = false;
    classify_operand_class(
        class, &mut int, &mut float, &mut vec, &mut csr, &mut sym, &mut frm, &mut fence, &mut vmask,
    );
    match hint {
        CandidateHint::IntReg => int,
        CandidateHint::FloatReg => float,
        CandidateHint::VecReg => vec,
        CandidateHint::Csr => csr,
        CandidateHint::Frm => frm,
        CandidateHint::Fence => fence,
        CandidateHint::Vmask => vmask,
        CandidateHint::SymbolOrImm => sym,
    }
}

pub(super) fn push_relocation_completions(
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    for (reloc, desc) in RELOCATION_FUNCTIONS {
        let rank = match_rank(reloc, typed_lower);
        items.push(make_item(
            (*reloc).to_owned(),
            CompletionItemKind::FUNCTION,
            Some("relocation modifier".to_owned()),
            Some(Documentation::String((*desc).to_owned())),
            format!("{rank}_3_{reloc}"),
            replace_range,
        ));
    }
}

pub(super) fn push_macro_param_completions(
    visible_syms: &[CompletionSymbol],
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for sym in visible_syms {
        if sym.kind == SymKind::MacroParam {
            let rank = match_rank(&sym.name, typed_lower);
            items.push(make_rich_item(
                sym.name.clone(),
                sym.name.clone(),
                format!("{}{sep}", sym.name),
                CompletionItemKind::VARIABLE,
                Some("macro parameter".to_owned()),
                None,
                None,
                format!("{rank}_0_{}", sym.name),
                replace_range,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_operand_class(
    class: &str,
    want_int: &mut bool,
    want_float: &mut bool,
    want_vec: &mut bool,
    want_csr: &mut bool,
    want_symbol_or_reloc: &mut bool,
    want_frm: &mut bool,
    want_fence: &mut bool,
    want_vmask: &mut bool,
) {
    if class == "csr_sysreg" {
        *want_csr = true;
    } else if class.starts_with("FPR") {
        if class.contains("INX") || class.contains("IN32X") {
            *want_int = true;
        } else {
            *want_float = true;
        }
    } else if class == "VMaskOp" {
        *want_vmask = true;
    } else if class.starts_with("VR") {
        *want_vec = true;
    } else if class.starts_with("GPR")
        || class.starts_with("SP")
        || class == "SR07"
        || class.starts_with("AnyReg")
    {
        *want_int = true;
    } else if class.starts_with("frmarg") {
        *want_frm = true;
    } else if class == "fencearg" {
        *want_fence = true;
    } else {
        *want_symbol_or_reloc = true;
    }
}

fn push_register_completions(
    db: &Database,
    class: RegisterClass,
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    let mut seen = BTreeSet::new();
    for reg in &db.raw.registers {
        if reg.class != class {
            continue;
        }
        for abi in &reg.abi_names {
            if seen.insert(abi.clone()) {
                let rank = match_rank(abi, typed_lower);
                items.push(make_rich_item(
                    abi.clone(),
                    abi.clone(),
                    format!("{abi}{sep}"),
                    CompletionItemKind::VARIABLE,
                    Some(reg.name.clone()),
                    reg.description.clone().map(Documentation::String),
                    None,
                    format!("{rank}_0_{:02}_{abi}", reg.number),
                    replace_range,
                ));
            }
        }
        if seen.insert(reg.name.clone()) {
            let detail = if reg.abi_names.is_empty() {
                None
            } else {
                Some(reg.abi_names.join(", "))
            };
            let rank = match_rank(&reg.name, typed_lower);
            items.push(make_rich_item(
                reg.name.clone(),
                reg.name.clone(),
                format!("{}{sep}", reg.name),
                CompletionItemKind::VARIABLE,
                detail,
                reg.description.clone().map(Documentation::String),
                None,
                format!("{rank}_1_{:02}_{}", reg.number, reg.name),
                replace_range,
            ));
        }
    }
}

fn push_csr_completions(
    db: &Database,
    march: &MarchSpec,
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for csr in &db.raw.csrs {
        if !csr.llvm_supported || !march.supports_csr(&csr.required_extensions, csr.xlen) {
            continue;
        }
        let rank = match_rank(&csr.name, typed_lower);
        items.push(make_rich_item(
            csr.name.clone(),
            csr.name.clone(),
            format!("{}{sep}", csr.name),
            CompletionItemKind::CONSTANT,
            Some(format!("0x{:03x}", csr.number)),
            csr.description.as_ref().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d.clone(),
                })
            }),
            None,
            format!("{rank}_0_{}", csr.name),
            replace_range,
        ));
    }
}
