use std::collections::BTreeSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTag, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, Range,
};
use serde::{Deserialize, Serialize};

use crate::context::MarchSpec;
use crate::data::{CsrEntry, Database, InstructionKind, InstructionVariant, RegisterClass};
use crate::index::{CompletionSymbol, SymKind};

use super::snippets::{
    build_instruction_snippet, deduplicate_and_sort_variants, directive_snippet,
    split_top_level_slots, syntax_args, trailing_operand_sep, variant_slot_count,
};
use super::{
    CompletionClientCaps, MAX_SYMBOL_CANDIDATES, RichItemSpec, make_item_with_data, make_rich_item,
    match_rank,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParenKind {
    None,
    Relocation,
    MemoryBase,
    Expression,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k")]
pub(super) enum ResolvePayload {
    Inst {
        m: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        s: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a: Option<String>,
    },
    Dir {
        n: String,
    },
    Reg {
        n: String,
    },
    Csr {
        n: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a: Option<String>,
    },
    Opt {
        n: String,
    },
    Reloc {
        n: String,
    },
    Frm {
        n: String,
    },
    Fence {
        n: String,
    },
}

pub(super) fn serialize_payload(payload: &ResolvePayload) -> Option<serde_json::Value> {
    serde_json::to_value(payload).ok()
}

pub(super) fn resolve_documentation(item: &CompletionItem, db: &Database) -> Option<Documentation> {
    let data = item.data.as_ref()?;
    let payload: ResolvePayload = serde_json::from_value(data.clone()).ok()?;
    match payload {
        ResolvePayload::Inst { m, s, a } => {
            let inst = db.instruction(&m)?;
            let march = a
                .as_deref()
                .map(MarchSpec::parse)
                .unwrap_or_else(|| MarchSpec::parse("rv64gc"));
            let md =
                crate::hover::format_instruction_completion_doc(inst, &march, db, s.as_deref());
            Some(markdown_doc(md))
        }
        ResolvePayload::Dir { n } => {
            let dir = db.directive(&n)?;
            Some(markdown_doc(crate::hover::format_directive_hover(dir, db)))
        }
        ResolvePayload::Reg { n } => {
            let reg = db.register(&n)?;
            Some(markdown_doc(crate::hover::format_register_hover(reg)))
        }
        ResolvePayload::Csr { n, a } => {
            let csr = db.csr(&n)?;
            let march = a.as_deref().map(MarchSpec::parse);
            Some(markdown_doc(crate::hover::format_csr_hover(
                csr,
                march.as_ref(),
                db,
            )))
        }
        ResolvePayload::Opt { n } => OPTION_SUBCOMMANDS
            .iter()
            .find(|(name, _)| *name == n)
            .map(|(_, desc)| Documentation::String((*desc).to_owned())),
        ResolvePayload::Reloc { n } => db
            .texts
            .relocations
            .iter()
            .find(|r| r.label == n)
            .map(|r| Documentation::String(r.description.clone())),
        ResolvePayload::Frm { n } => ROUNDING_MODES
            .iter()
            .find(|(name, _)| *name == n)
            .map(|(_, desc)| Documentation::String((*desc).to_owned())),
        ResolvePayload::Fence { n } => FENCE_ARGS
            .iter()
            .find(|(name, _)| *name == n)
            .map(|(_, desc)| Documentation::String((*desc).to_owned())),
    }
}

fn markdown_doc(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

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

pub(crate) const ROUNDING_MODES: &[(&str, &str)] = &[
    ("rne", "Round to Nearest, ties to Even"),
    ("rtz", "Round towards Zero"),
    ("rdn", "Round Down (towards -infinity)"),
    ("rup", "Round Up (towards +infinity)"),
    ("rmm", "Round to Nearest, ties to Max Magnitude"),
    ("dyn", "Dynamic rounding mode (use frm CSR)"),
];

pub(crate) const FENCE_ARGS: &[(&str, &str)] = &[
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
        let cat_tier = if inst.deprecated { '3' } else { '1' };
        let tags = inst.deprecated.then(|| vec![CompletionItemTag::DEPRECATED]);
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
            let data = serialize_payload(&ResolvePayload::Inst {
                m: inst.mnemonic.clone(),
                s: None,
                a: Some(march.raw.clone()),
            });
            let sort = format!("{rank}_{cat_tier}_{}", inst.mnemonic);
            items.push(make_rich_item(RichItemSpec {
                label: inst.mnemonic.clone(),
                filter_text: inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                tags,
                data,
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                sort_text: sort,
                replace_range,
            }));
            continue;
        }

        for (idx, v) in unique_variants.drain(..).enumerate() {
            let args_str = syntax_args(&inst.mnemonic, &v.syntax);
            let (new_text, format) = build_instruction_snippet(&inst.mnemonic, args_str, v);
            let label = v.syntax.clone();
            let detail = inst.long_name.clone();
            let data = serialize_payload(&ResolvePayload::Inst {
                m: inst.mnemonic.clone(),
                s: Some(v.syntax.clone()),
                a: Some(march.raw.clone()),
            });
            let sort = format!("{rank}_{cat_tier}_{}_{idx:02}", inst.mnemonic);

            items.push(make_rich_item(RichItemSpec {
                label,
                filter_text: inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                tags: tags.clone(),
                data,
                insert_text_format: Some(format),
                sort_text: sort,
                replace_range,
            }));
        }
    }
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
        let dir_data = serialize_payload(&ResolvePayload::Dir {
            n: dir.name.clone(),
        });

        if caps.snippet_support && dir.name == ".option" {
            items.push(make_rich_item(RichItemSpec {
                label: ".option push ... pop".to_owned(),
                filter_text: ".option".to_owned(),
                new_text: ".option push\n.option ${1:norvc}\n$0\n.option pop".to_owned(),
                kind: CompletionItemKind::SNIPPET,
                detail: None,
                tags: None,
                data: dir_data.clone(),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: format!("{rank}_2_.option_0"),
                replace_range,
            }));

            let inline_text = if next_is_ws {
                ".option".to_owned()
            } else {
                ".option ".to_owned()
            };
            items.push(make_rich_item(RichItemSpec {
                label: ".option <opt>".to_owned(),
                filter_text: ".option".to_owned(),
                new_text: inline_text,
                kind: CompletionItemKind::KEYWORD,
                detail: None,
                tags: None,
                data: dir_data,
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                sort_text: format!("{rank}_2_.option_1"),
                replace_range,
            }));
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
        items.push(make_rich_item(RichItemSpec {
            label,
            filter_text: dir.name.clone(),
            new_text,
            kind: CompletionItemKind::KEYWORD,
            detail: dir.arguments.clone(),
            tags: None,
            data: dir_data,
            insert_text_format: Some(format),
            sort_text: sort,
            replace_range,
        }));
    }
}

pub(super) fn push_option_arch_completions(
    db: &Database,
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    let prefix = if typed_lower.starts_with('-') {
        '-'
    } else {
        '+'
    };
    let ext_typed = typed_lower.trim_start_matches(['+', '-']);
    for ext in &db.raw.extensions {
        let label = format!("{prefix}{}", ext.march_name);
        let rank = match_rank(&ext.march_name, ext_typed);
        items.push(make_rich_item(RichItemSpec {
            label: label.clone(),
            filter_text: label.clone(),
            new_text: label,
            kind: CompletionItemKind::VALUE,
            detail: ext.description.clone(),
            tags: None,
            data: None,
            insert_text_format: None,
            sort_text: format!("{rank}_0_{}", ext.march_name),
            replace_range,
        }));
    }
}

pub(super) fn has_memory_operand_at(
    db: &Database,
    march: &MarchSpec,
    mnemonic: &str,
    operand_index: usize,
) -> bool {
    let Some(inst) = db.instruction(mnemonic) else {
        return false;
    };
    let matching: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
        .collect();
    let pool: Vec<_> = if matching.is_empty() {
        inst.variants.iter().collect()
    } else {
        matching
    };
    pool.into_iter().any(|v| {
        let slots = split_top_level_slots(syntax_args(mnemonic, &v.syntax));
        slots
            .get(operand_index)
            .is_some_and(|slot| slot.contains('(') && slot.contains(')'))
    })
}

pub(super) struct OperandQuery<'a> {
    pub db: &'a Database,
    pub march: &'a MarchSpec,
    pub mnemonic: &'a str,
    pub operand_index: usize,
    pub paren_kind: ParenKind,
    pub next_is_comma: bool,
    pub visible_syms: &'a [CompletionSymbol],
    pub in_macro_body: bool,
    pub typed_lower: &'a str,
    pub replace_range: Range,
}

#[derive(Default)]
struct OperandDemand {
    int_reg: bool,
    float_reg: bool,
    vec_reg: bool,
    csr: bool,
    symbol_or_reloc: bool,
    frm: bool,
    fence: bool,
    vmask: bool,
}

pub(super) fn collect_operand_completions(
    q: OperandQuery<'_>,
    items: &mut Vec<CompletionItem>,
    is_incomplete: &mut bool,
) {
    let (demand, active_variants) = infer_operand_demand(&q);
    let sep_for = |hint: CandidateHint| {
        trailing_operand_sep(
            &active_variants,
            q.mnemonic,
            q.operand_index,
            q.paren_kind != ParenKind::None,
            q.next_is_comma,
            hint,
        )
    };

    if demand.int_reg {
        push_register_completions(
            q.db,
            RegisterClass::Integer,
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::IntReg),
            items,
        );
    }
    if demand.float_reg {
        push_register_completions(
            q.db,
            RegisterClass::Float,
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::FloatReg),
            items,
        );
    }
    if demand.vec_reg {
        push_register_completions(
            q.db,
            RegisterClass::Vector,
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::VecReg),
            items,
        );
    }
    if demand.csr {
        push_csr_completions(
            q.db,
            q.march,
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::Csr),
            items,
        );
    }
    if demand.frm {
        push_frm_completions(
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::Frm),
            items,
        );
    }
    if demand.fence {
        push_fence_completions(
            q.typed_lower,
            q.replace_range,
            sep_for(CandidateHint::Fence),
            items,
        );
    }
    if demand.vmask {
        let rank = match_rank("v0.t", q.typed_lower);
        let vmask_sep = sep_for(CandidateHint::Vmask);
        items.push(make_rich_item(RichItemSpec {
            label: "v0.t".to_owned(),
            filter_text: "v0.t".to_owned(),
            new_text: format!("v0.t{vmask_sep}"),
            kind: CompletionItemKind::VALUE,
            detail: Some("vector mask register".to_owned()),
            tags: None,
            data: None,
            insert_text_format: None,
            sort_text: format!("{rank}_0_v0.t"),
            replace_range: q.replace_range,
        }));
    }
    if demand.symbol_or_reloc {
        push_visible_symbol_completions(
            &q,
            sep_for(CandidateHint::SymbolOrImm),
            items,
            is_incomplete,
        );
        push_relocation_completions(q.db, q.typed_lower, q.replace_range, items);
    }
    if q.in_macro_body {
        let param_sep = if active_variants.is_empty() {
            ""
        } else {
            sep_for(CandidateHint::IntReg)
        };
        push_macro_param_completions(
            q.visible_syms,
            q.typed_lower,
            q.replace_range,
            param_sep,
            items,
        );
    }
}

fn infer_operand_demand<'a>(
    q: &'a OperandQuery<'a>,
) -> (OperandDemand, Vec<&'a InstructionVariant>) {
    let mut demand = OperandDemand::default();
    let mut active_variants = Vec::new();
    match q.paren_kind {
        ParenKind::Relocation => {
            demand.symbol_or_reloc = true;
            return (demand, active_variants);
        }
        ParenKind::MemoryBase => {
            demand.int_reg = true;
            return (demand, active_variants);
        }
        ParenKind::Expression => {
            demand.symbol_or_reloc = true;
            if q.db.instruction(q.mnemonic).is_none() && !q.mnemonic.starts_with('.') {
                demand.int_reg = true;
            }
            return (demand, active_variants);
        }
        ParenKind::None => {}
    }
    if let Some(inst) = q.db.instruction(q.mnemonic) {
        let variants: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| q.march.supports_extensions(&v.required_extensions, v.xlen))
            .collect();
        active_variants = if variants.is_empty() {
            inst.variants.iter().collect()
        } else {
            variants
        };

        let mut matched_any = false;
        for &v in &active_variants {
            if let Some(op) = v.operands.get(q.operand_index) {
                matched_any = true;
                classify_operand_class(&op.class, &mut demand);
            }
        }
        if !matched_any && q.operand_index == 0 {
            demand.int_reg = true;
        }
    } else {
        demand.symbol_or_reloc = true;
        if !q.mnemonic.starts_with('.') {
            demand.int_reg = true;
        }
    }
    (demand, active_variants)
}

fn push_frm_completions(
    typed_lower: &str,
    replace_range: Range,
    frm_sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for (name, _) in ROUNDING_MODES {
        let rank = match_rank(name, typed_lower);
        items.push(make_rich_item(RichItemSpec {
            label: (*name).to_owned(),
            filter_text: (*name).to_owned(),
            new_text: format!("{name}{frm_sep}"),
            kind: CompletionItemKind::ENUM_MEMBER,
            detail: Some("rounding mode".to_owned()),
            tags: None,
            data: serialize_payload(&ResolvePayload::Frm {
                n: (*name).to_owned(),
            }),
            insert_text_format: None,
            sort_text: format!("{rank}_0_{name}"),
            replace_range,
        }));
    }
}

fn push_fence_completions(
    typed_lower: &str,
    replace_range: Range,
    fence_sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for (name, _) in FENCE_ARGS {
        let rank = match_rank(name, typed_lower);
        items.push(make_rich_item(RichItemSpec {
            label: (*name).to_owned(),
            filter_text: (*name).to_owned(),
            new_text: format!("{name}{fence_sep}"),
            kind: CompletionItemKind::ENUM_MEMBER,
            detail: Some("fence ordering".to_owned()),
            tags: None,
            data: serialize_payload(&ResolvePayload::Fence {
                n: (*name).to_owned(),
            }),
            insert_text_format: None,
            sort_text: format!("{rank}_0_{name}"),
            replace_range,
        }));
    }
}

fn push_visible_symbol_completions(
    q: &OperandQuery<'_>,
    sym_sep: &str,
    items: &mut Vec<CompletionItem>,
    is_incomplete: &mut bool,
) {
    let mut syms: Vec<&CompletionSymbol> = q
        .visible_syms
        .iter()
        .filter(|s| s.kind != SymKind::MacroParam)
        .collect();
    if syms.len() > MAX_SYMBOL_CANDIDATES {
        *is_incomplete = true;
        if !q.typed_lower.is_empty() {
            syms.retain(|s| s.name.to_ascii_lowercase().contains(q.typed_lower));
        }
        syms.sort_by_key(|s| {
            (
                match_rank(&s.name, q.typed_lower),
                s.name.to_ascii_lowercase(),
            )
        });
        syms.truncate(MAX_SYMBOL_CANDIDATES);
    }
    for sym in syms {
        let kind = match sym.kind {
            SymKind::Equ | SymKind::Define => CompletionItemKind::CONSTANT,
            _ => CompletionItemKind::REFERENCE,
        };
        let rank = match_rank(&sym.name, q.typed_lower);
        let sort = format!("{rank}_2_{}", sym.name.to_ascii_lowercase());
        items.push(make_rich_item(RichItemSpec {
            label: sym.name.clone(),
            filter_text: sym.name.clone(),
            new_text: format!("{}{sym_sep}", sym.name),
            kind,
            detail: sym.detail.clone(),
            tags: None,
            data: None,
            insert_text_format: None,
            sort_text: sort,
            replace_range: q.replace_range,
        }));
    }
}

pub(super) fn operand_class_matches_hint(class: &str, hint: CandidateHint) -> bool {
    let mut demand = OperandDemand::default();
    classify_operand_class(class, &mut demand);
    match hint {
        CandidateHint::IntReg => demand.int_reg,
        CandidateHint::FloatReg => demand.float_reg,
        CandidateHint::VecReg => demand.vec_reg,
        CandidateHint::Csr => demand.csr,
        CandidateHint::Frm => demand.frm,
        CandidateHint::Fence => demand.fence,
        CandidateHint::Vmask => demand.vmask,
        CandidateHint::SymbolOrImm => demand.symbol_or_reloc,
    }
}

pub(super) fn push_relocation_completions(
    db: &Database,
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    for reloc in &db.texts.relocations {
        let rank = match_rank(&reloc.label, typed_lower);
        items.push(make_item_with_data(
            reloc.label.clone(),
            CompletionItemKind::FUNCTION,
            Some("relocation modifier".to_owned()),
            serialize_payload(&ResolvePayload::Reloc {
                n: reloc.label.clone(),
            }),
            format!("{rank}_3_{}", reloc.label),
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
            items.push(make_rich_item(RichItemSpec {
                label: sym.name.clone(),
                filter_text: sym.name.clone(),
                new_text: format!("{}{sep}", sym.name),
                kind: CompletionItemKind::VARIABLE,
                detail: Some("macro parameter".to_owned()),
                tags: None,
                data: None,
                insert_text_format: None,
                sort_text: format!("{rank}_0_{}", sym.name),
                replace_range,
            }));
        }
    }
}

fn classify_operand_class(class: &str, demand: &mut OperandDemand) {
    if class == "csr_sysreg" {
        demand.csr = true;
    } else if class.starts_with("FPR") {
        if class.contains("INX") || class.contains("IN32X") {
            demand.int_reg = true;
        } else {
            demand.float_reg = true;
        }
    } else if class == "VMaskOp" {
        demand.vmask = true;
    } else if class.starts_with("VR") {
        demand.vec_reg = true;
    } else if class.starts_with("GPR")
        || class.starts_with("SP")
        || class.starts_with("BasePtr")
        || class == "SR07"
        || class == "YGPR"
        || class.starts_with("AnyReg")
    {
        demand.int_reg = true;
    } else if class.starts_with("frmarg") {
        demand.frm = true;
    } else if class == "fencearg" {
        demand.fence = true;
    } else {
        demand.symbol_or_reloc = true;
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
        let reg_data = serialize_payload(&ResolvePayload::Reg {
            n: reg.name.clone(),
        });
        for abi in &reg.abi_names {
            if seen.insert(abi.clone()) {
                let rank = match_rank(abi, typed_lower);
                items.push(make_rich_item(RichItemSpec {
                    label: abi.clone(),
                    filter_text: abi.clone(),
                    new_text: format!("{abi}{sep}"),
                    kind: CompletionItemKind::VARIABLE,
                    detail: Some(reg.name.clone()),
                    tags: None,
                    data: reg_data.clone(),
                    insert_text_format: None,
                    sort_text: format!("{rank}_0_{:02}_{abi}", reg.number),
                    replace_range,
                }));
            }
        }
        if seen.insert(reg.name.clone()) {
            let detail = (!reg.abi_names.is_empty()).then(|| reg.abi_names.join(", "));
            let rank = match_rank(&reg.name, typed_lower);
            items.push(make_rich_item(RichItemSpec {
                label: reg.name.clone(),
                filter_text: reg.name.clone(),
                new_text: format!("{}{sep}", reg.name),
                kind: CompletionItemKind::VARIABLE,
                detail,
                tags: None,
                data: reg_data,
                insert_text_format: None,
                sort_text: format!("{rank}_1_{:02}_{}", reg.number, reg.name),
                replace_range,
            }));
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
        items.push(make_rich_item(RichItemSpec {
            label: csr.name.clone(),
            filter_text: csr.name.clone(),
            new_text: format!("{}{sep}", csr.name),
            kind: CompletionItemKind::CONSTANT,
            detail: Some(format_csr_detail(csr)),
            tags: None,
            data: serialize_payload(&ResolvePayload::Csr {
                n: csr.name.clone(),
                a: Some(march.raw.clone()),
            }),
            insert_text_format: None,
            sort_text: format!("{rank}_0_{}", csr.name),
            replace_range,
        }));
    }
}

const ELF_SECTION_NAMES: &[(&str, &str)] = &[
    (".text", "Executable code section"),
    (".data", "Initialized writable data section"),
    (".rodata", "Read-only data section"),
    (".bss", "Zero-initialized writable data section"),
    (".init", "Initialization code section"),
    (".fini", "Finalization code section"),
    (".tdata", "Initialized thread-local data section"),
    (".tbss", "Zero-initialized thread-local data section"),
    (".note.GNU-stack", "Stack executability note section"),
    (".text.startup", "Startup code section"),
    (".rodata.str1.1", "Mergeable 1-byte aligned string literals"),
];

const ELF_SECTION_FLAGS: &[(&str, &str)] = &[
    ("ax", "Allocatable and executable"),
    ("aw", "Allocatable and writable"),
    ("a", "Allocatable (read-only)"),
    ("aMS", "Allocatable, mergeable strings"),
    ("awT", "Allocatable, writable, TLS"),
    ("axG", "Allocatable, executable, COMDAT group"),
];

const ELF_SECTION_TYPES: &[(&str, &str)] = &[
    ("@progbits", "Section contains program-defined data"),
    ("@nobits", "Section occupies no file space (e.g. .bss)"),
    ("@note", "Section contains ELF note records"),
    ("@init_array", "Array of initialization function pointers"),
    ("@fini_array", "Array of termination function pointers"),
];

pub(super) fn push_section_arg_completions(
    arg_index: usize,
    in_quotes: bool,
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    match arg_index {
        0 => {
            for &(name, desc) in ELF_SECTION_NAMES {
                let rank = match_rank(name, typed_lower);
                items.push(make_rich_item(RichItemSpec {
                    label: name.to_owned(),
                    filter_text: name.to_owned(),
                    new_text: name.to_owned(),
                    kind: CompletionItemKind::MODULE,
                    detail: Some(desc.to_owned()),
                    tags: None,
                    data: None,
                    insert_text_format: None,
                    sort_text: format!("{rank}_0_{name}"),
                    replace_range,
                }));
            }
        }
        1 => {
            let trimmed = typed_lower.trim_matches('"');
            for &(flags, desc) in ELF_SECTION_FLAGS {
                let label = format!("\"{flags}\"");
                let new_text = if in_quotes {
                    flags.to_owned()
                } else {
                    label.clone()
                };
                let rank = match_rank(flags, trimmed);
                items.push(make_rich_item(RichItemSpec {
                    label,
                    filter_text: flags.to_owned(),
                    new_text,
                    kind: CompletionItemKind::ENUM_MEMBER,
                    detail: Some(desc.to_owned()),
                    tags: None,
                    data: None,
                    insert_text_format: None,
                    sort_text: format!("{rank}_0_{flags}"),
                    replace_range,
                }));
            }
        }
        _ => {
            for &(ty, desc) in ELF_SECTION_TYPES {
                let rank = match_rank(ty, typed_lower);
                items.push(make_rich_item(RichItemSpec {
                    label: ty.to_owned(),
                    filter_text: ty.to_owned(),
                    new_text: ty.to_owned(),
                    kind: CompletionItemKind::TYPE_PARAMETER,
                    detail: Some(desc.to_owned()),
                    tags: None,
                    data: None,
                    insert_text_format: None,
                    sort_text: format!("{rank}_0_{ty}"),
                    replace_range,
                }));
            }
        }
    }
}

fn format_csr_detail(csr: &CsrEntry) -> String {
    match csr.privilege_label() {
        Some(label) => format!("0x{:03x} · {label}", csr.number),
        None => format!("0x{:03x}", csr.number),
    }
}
