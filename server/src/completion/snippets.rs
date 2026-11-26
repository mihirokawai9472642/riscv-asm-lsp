use std::collections::BTreeSet;

use lsp_types::InsertTextFormat;

use crate::data::InstructionVariant;

use super::candidates::{CandidateHint, operand_class_matches_hint};

pub(crate) fn syntax_args<'a>(mnemonic: &str, syntax: &'a str) -> &'a str {
    let trimmed = syntax.trim();
    if let Some(rest) = trimmed.strip_prefix(mnemonic) {
        let next = rest.chars().next();
        if next.is_none_or(char::is_whitespace) {
            return rest.trim();
        }
    }
    trimmed
        .split_once(char::is_whitespace)
        .map_or("", |(_, r)| r.trim())
}

pub(crate) fn is_special_variant(mnemonic: &str, v: &InstructionVariant) -> bool {
    let is_bare_symbol_exempt = matches!(mnemonic.to_ascii_lowercase().as_str(), "la" | "lla");
    v.operands.iter().any(|o| {
        matches!(
            o.class.as_str(),
            "tprel_add_symbol" | "tlsdesc_call_symbol" | "call_symbol" | "pseudo_jump_symbol"
        ) || (!is_bare_symbol_exempt && o.class == "bare_symbol")
    }) || v.syntax.contains(".insn")
}

fn is_zfinx_variant(v: &InstructionVariant) -> bool {
    v.required_extensions.iter().any(|ext| {
        ext.split('|').any(|part| {
            matches!(
                part.trim().to_ascii_lowercase().as_str(),
                "zfinx" | "zdinx" | "zhinx" | "zhinxmin"
            )
        })
    }) || v
        .operands
        .iter()
        .any(|o| o.class.ends_with("INX") || o.class.ends_with("IN32X"))
}

pub(crate) fn deduplicate_and_sort_variants<'a>(
    mnemonic: &str,
    variants: &[&'a InstructionVariant],
) -> Vec<&'a InstructionVariant> {
    // Sort before deduplicating so canonical F/D variants win over Z*inx duplicates.
    let mut sorted = variants.to_vec();
    sorted.sort_by_key(|v| variant_sort_key(mnemonic, v));

    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(sorted.len());
    for v in sorted {
        let norm = v.syntax.split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.insert(norm) {
            out.push(v);
        }
    }
    out
}

pub(super) fn variant_sort_key(
    mnemonic: &str,
    v: &InstructionVariant,
) -> (u8, u8, u8, u8, usize, String) {
    let args = syntax_args(mnemonic, &v.syntax);
    // Order variants of a mnemonic (Item 2):
    // 0: primary instruction itself (no non-self `alias_of` and not a special relocation/symbol form),
    // 1: LLVM alias (e.g. `add rd, rs1, imm12` -> `addi`),
    // 2: special TLS / symbol / relocation forms (`tprel_add_symbol`, `bare_symbol`, `.insn`).
    let is_special = is_special_variant(mnemonic, v);
    let is_alias = v
        .alias_of
        .as_deref()
        .is_some_and(|a| !a.eq_ignore_ascii_case(mnemonic));
    let tier = if is_special {
        2
    } else if is_alias {
        1
    } else {
        0
    };

    let zfinx_rank = u8::from(is_zfinx_variant(v));
    let has_reg = v.operands.iter().any(|o| is_register_class(&o.class));
    let reg_score = u8::from(!args.is_empty() && !has_reg);
    let shorthand_mem_score = u8::from(
        split_top_level_slots(args)
            .iter()
            .any(|slot| split_mem_slot(slot).is_some_and(|(before, _)| before.is_empty())),
    );
    (
        tier,
        zfinx_rank,
        reg_score,
        shorthand_mem_score,
        v.operands.len(),
        v.syntax.clone(),
    )
}

fn is_register_class(class: &str) -> bool {
    class == "SP"
        || class.starts_with("GPR")
        || class.starts_with("BasePtr")
        || class == "SR07"
        || class == "YGPR"
        || class.starts_with("AnyReg")
        || class.starts_with("FPR")
        || class.starts_with("VR")
}

pub(super) fn build_instruction_snippet(
    mnemonic: &str,
    args_str: &str,
    v: &InstructionVariant,
) -> (String, InsertTextFormat) {
    if args_str.is_empty() {
        return (mnemonic.to_owned(), InsertTextFormat::PLAIN_TEXT);
    }

    let slots = split_top_level_slots(args_str);
    let mut tab_idx = 1usize;
    let mut formatted_slots = Vec::with_capacity(slots.len());

    for slot in slots {
        if let Some((before_paren, inside_paren)) = split_mem_slot(slot) {
            if before_paren.is_empty() {
                formatted_slots.push(format!("(${{{tab_idx}:{inside_paren}}})"));
                tab_idx += 1;
            } else {
                let first = tab_idx;
                let second = tab_idx + 1;
                tab_idx += 2;
                formatted_slots.push(format!("${{{first}:0}}(${{{second}:{inside_paren}}})"));
            }
        } else if v.operands.iter().any(|o| o.name == slot) {
            formatted_slots.push(format!("${{{tab_idx}:{slot}}}"));
            tab_idx += 1;
        } else {
            // Fixed literal operand such as `x0` in `c.addi x0, imm`
            formatted_slots.push(slot.to_owned());
        }
    }

    if tab_idx == 1 {
        return (
            format!("{mnemonic} {args_str}"),
            InsertTextFormat::PLAIN_TEXT,
        );
    }
    (
        format!("{mnemonic} {}$0", formatted_slots.join(", ")),
        InsertTextFormat::SNIPPET,
    )
}

fn split_mem_slot(slot: &str) -> Option<(&str, &str)> {
    let open = slot.find('(')?;
    let close = slot[open + 1..].find(')')?;
    let inside = slot[open + 1..open + 1 + close].trim();
    if inside.is_empty() {
        return None;
    }
    Some((slot[..open].trim(), inside))
}

pub(crate) fn split_top_level_slots(args_str: &str) -> Vec<&str> {
    if args_str.trim().is_empty() {
        return Vec::new();
    }
    let mut slots = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in args_str.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let part = args_str[start..idx].trim();
                if !part.is_empty() {
                    slots.push(part);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = args_str[start..].trim();
    if !tail.is_empty() {
        slots.push(tail);
    }
    slots
}

pub(crate) fn variant_slot_count(mnemonic: &str, syntax: &str) -> usize {
    split_top_level_slots(syntax_args(mnemonic, syntax)).len()
}

pub(super) fn directive_snippet(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        ".macro" => Some((".macro ${1:name} ${2:args}\n\t$0\n.endm", "name args")),
        ".rept" => Some((".rept ${1:count}\n\t$0\n.endr", "count")),
        ".irp" => Some((".irp ${1:sym}, ${2:values}\n\t$0\n.endr", "sym, values")),
        ".if" => Some((".if ${1:expr}\n\t$0\n.endif", "expr")),
        ".equ" => Some((".equ ${1:NAME}, ${2:value}$0", "NAME, value")),
        ".section" => Some((".section ${1:.text}$0", ".text")),
        _ => None,
    }
}

pub(super) fn trailing_operand_sep(
    active_variants: &[&InstructionVariant],
    mnemonic: &str,
    operand_index: usize,
    inside_parens: bool,
    next_is_comma: bool,
    hint: CandidateHint,
) -> &'static str {
    if inside_parens || next_is_comma || active_variants.is_empty() {
        return "";
    }
    let sorted = deduplicate_and_sort_variants(mnemonic, active_variants);
    let matched = sorted
        .iter()
        .copied()
        .find(|v| {
            v.operands
                .get(operand_index)
                .is_some_and(|op| operand_class_matches_hint(&op.class, hint))
        })
        .or_else(|| sorted.first().copied());
    let Some(best) = matched else {
        return "";
    };
    let slots = variant_slot_count(mnemonic, &best.syntax);
    if operand_index + 1 < slots { ", " } else { "" }
}
