use lsp_types::{
    Documentation, ParameterInformation, ParameterLabel, Position, SignatureHelp,
    SignatureInformation,
};

use crate::completion::snippets::{
    deduplicate_and_sort_variants, split_top_level_slots, syntax_args, variant_slot_count,
};
use crate::context::{CompileContext, effective_options_at_line};
use crate::data::Database;
use crate::document::Document;
use crate::hover::operands::describe_operand_class;

pub fn provide_signature_help(
    doc: &Document,
    pos: Position,
    db: &Database,
    ctx: &mut CompileContext,
) -> Option<SignatureHelp> {
    let file_ctx = ctx.for_uri(&doc.uri);
    let offset = doc.line_index.position_to_offset(&doc.text, pos);
    if crate::line::is_offset_in_comment(
        &doc.text,
        doc.tree.as_ref(),
        offset,
        file_ctx.preprocessed,
    ) {
        return None;
    }

    let (line_start, _, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
    let line_prefix = doc.text.get(line_start..offset)?;
    let starts_in_block =
        crate::line::in_block_comment_before(&doc.text, line_start, file_ctx.preprocessed);
    let active =
        crate::line::active_statement_prefix_with_block_state(line_prefix, starts_in_block);
    let ws_pos = active.find(char::is_whitespace)?;
    let head = active[..ws_pos].trim();
    if head.is_empty() {
        return None;
    }

    let inst = db.instruction(head)?;
    if !inst.llvm_supported {
        return None;
    }

    let args = active[ws_pos..].trim_start();
    let (operand_index, _) = crate::line::count_operand_position(args);
    let (march, _) = effective_options_at_line(&doc.text, pos.line as usize, &file_ctx.march);

    let ext_matching: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
        .collect();
    let pool: Vec<_> = if !ext_matching.is_empty() {
        ext_matching
    } else {
        let xlen_matching: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| march.supports_xlen(v.xlen))
            .collect();
        if xlen_matching.is_empty() {
            inst.variants.iter().collect()
        } else {
            xlen_matching
        }
    };
    let variants = deduplicate_and_sort_variants(&inst.mnemonic, &pool);
    if variants.is_empty() {
        return None;
    }

    let doc_text = inst.summary.clone();
    let mut signatures = Vec::with_capacity(variants.len());
    let mut active_signature: Option<u32> = None;

    for (sig_idx, v) in variants.iter().enumerate() {
        let slots = split_top_level_slots(syntax_args(&inst.mnemonic, &v.syntax));
        if active_signature.is_none() && operand_index < slots.len() {
            active_signature = Some(sig_idx as u32);
        }

        let mut params = Vec::with_capacity(slots.len());
        let mut search_from = 0usize;
        for (slot_idx, slot) in slots.iter().enumerate() {
            let label = if let Some(rel) = v.syntax[search_from..].find(slot) {
                let start_b = search_from + rel;
                let end_b = start_b + slot.len();
                search_from = end_b;
                let start_u16 = v.syntax[..start_b]
                    .chars()
                    .map(|c| c.len_utf16() as u32)
                    .sum();
                let end_u16 = start_u16 + slot.chars().map(|c| c.len_utf16() as u32).sum::<u32>();
                ParameterLabel::LabelOffsets([start_u16, end_u16])
            } else {
                ParameterLabel::Simple((*slot).to_owned())
            };

            let op_desc = v
                .operands
                .iter()
                .find(|o| o.name == *slot || slot.contains(&format!("({})", o.name)))
                .or_else(|| v.operands.get(slot_idx))
                .and_then(|o| describe_operand_class(&o.class, db))
                .map(Documentation::String);

            params.push(ParameterInformation {
                label,
                documentation: op_desc,
            });
        }

        let slot_count = variant_slot_count(&inst.mnemonic, &v.syntax);
        let sig_active_param =
            (slot_count > 0).then(|| operand_index.min(slot_count.saturating_sub(1)) as u32);

        signatures.push(SignatureInformation {
            label: v.syntax.clone(),
            documentation: doc_text.clone().map(Documentation::String),
            parameters: Some(params),
            active_parameter: sig_active_param,
        });
    }

    let chosen_sig = active_signature.unwrap_or(0);
    let active_param = signatures
        .get(chosen_sig as usize)
        .and_then(|s| s.active_parameter);

    Some(SignatureHelp {
        signatures,
        active_signature: Some(chosen_sig),
        active_parameter: active_param,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;

    #[test]
    fn signature_help_tracks_active_operand_and_variants() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("sig.S");
        let doc = store.open(uri, 1, "    addi a0, a1, \n// addi a0, \n".to_owned());
        let mut ctx = CompileContext::new(None, Config::default());

        let sig = provide_signature_help(doc, Position::new(0, 17), &db, &mut ctx)
            .expect("signature help");
        assert!(!sig.signatures.is_empty());
        assert_eq!(sig.signatures[0].label, "addi rd, rs1, imm12");
        assert_eq!(sig.active_parameter, Some(2));

        // Suppressed inside comment
        assert!(provide_signature_help(doc, Position::new(1, 9), &db, &mut ctx).is_none());
    }
}
