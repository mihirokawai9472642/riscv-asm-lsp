use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};

use crate::context::CompileContext;
use crate::data::Database;
use crate::document::Document;
use crate::hover::parse_csr_number;
use crate::index::{SymKind, WorkspaceIndex};

const MAX_VALUE_HINT_LEN: usize = 24;

pub fn provide_inlay_hints(
    doc: &Document,
    range: Range,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
) -> Option<Vec<InlayHint>> {
    let file_ctx = ctx.for_uri(&doc.uri);
    index.sync_preprocessed(&doc.uri, file_ctx.preprocessed);

    let line_count = doc.line_index.line_count();
    if line_count == 0 {
        return None;
    }
    let start_line = (range.start.line as usize).min(line_count.saturating_sub(1));
    let end_line = (range.end.line as usize).min(line_count.saturating_sub(1));
    if start_line > end_line {
        return None;
    }

    let mut hints = Vec::new();
    let mut in_block = false;
    let mut line_start = 0usize;
    let mut hctx = HintContext {
        doc,
        db,
        ctx,
        index,
    };

    for (line_idx, raw_line) in doc.text.split_inclusive('\n').enumerate() {
        let raw_len = raw_line.len();
        if line_idx > end_line {
            break;
        }
        let code = crate::line::strip_comments_with_state(raw_line, &mut in_block, false);
        if line_idx < start_line || code.trim().is_empty() {
            line_start += raw_len;
            continue;
        }
        if file_ctx.preprocessed
            && crate::preprocessor::classify_hash_line(raw_line)
                .is_some_and(|k| k.is_directive_or_marker())
        {
            line_start += raw_len;
            continue;
        }

        collect_line_hints(&code, line_start, raw_line, &mut hctx, &mut hints);
        line_start += raw_len;
    }

    (!hints.is_empty()).then_some(hints)
}

struct HintContext<'a> {
    doc: &'a Document,
    db: &'a Database,
    ctx: &'a mut CompileContext,
    index: &'a mut WorkspaceIndex,
}

fn collect_line_hints(
    code: &str,
    line_start: usize,
    raw_line: &str,
    hctx: &mut HintContext<'_>,
    hints: &mut Vec<InlayHint>,
) {
    let mut stmt_offset = 0usize;
    for stmt in split_statements_with_offsets(code) {
        let leading_label_end = skip_label_prefix(stmt.text);
        let active = &stmt.text[leading_label_end..];
        let trimmed_lead = active.len() - active.trim_start().len();
        let stmt_trimmed = active.trim();
        let active_rel = stmt.start + leading_label_end + trimmed_lead;

        if let Some(ws) = stmt_trimmed.find(char::is_whitespace) {
            let head = stmt_trimmed[..ws].trim();
            if let Some(inst) = hctx.db.instruction(head) {
                let after_head = &stmt_trimmed[ws..];
                let args_ws = after_head.len() - after_head.trim_start().len();
                let args = after_head.trim_start();
                let args_rel = active_rel + ws + args_ws;
                let has_csr_op = inst
                    .variants
                    .iter()
                    .any(|v| v.operands.iter().any(|o| o.class == "csr_sysreg"));

                for (op_idx, op_slice) in split_top_level_with_offsets(args).into_iter().enumerate()
                {
                    let is_csr_slot = has_csr_op
                        && inst.variants.iter().any(|v| {
                            v.operands
                                .get(op_idx)
                                .is_some_and(|o| o.class == "csr_sysreg")
                        });
                    for tok in extract_tokens_in_slice(op_slice.text) {
                        let abs_start = line_start + args_rel + op_slice.start + tok.start;
                        let abs_end = line_start + args_rel + op_slice.start + tok.end;
                        if abs_end > line_start + raw_line.len() {
                            continue;
                        }
                        if is_csr_slot {
                            if let Some(csr) = hctx.db.csr(tok.text) {
                                hints.push(InlayHint {
                                    position: hctx
                                        .doc
                                        .line_index
                                        .offset_to_position(&hctx.doc.text, abs_end),
                                    label: InlayHintLabel::String(format!(
                                        ": 0x{:03x}",
                                        csr.number
                                    )),
                                    kind: Some(InlayHintKind::TYPE),
                                    text_edits: None,
                                    tooltip: None,
                                    padding_left: Some(false),
                                    padding_right: Some(false),
                                    data: None,
                                });
                                continue;
                            }
                            if let Some(num) = parse_csr_number(tok.text)
                                && let Some(csr) = hctx.db.csr_by_number(num)
                            {
                                hints.push(InlayHint {
                                    position: hctx
                                        .doc
                                        .line_index
                                        .offset_to_position(&hctx.doc.text, abs_end),
                                    label: InlayHintLabel::String(format!(": {}", csr.name)),
                                    kind: Some(InlayHintKind::TYPE),
                                    text_edits: None,
                                    tooltip: None,
                                    padding_left: Some(false),
                                    padding_right: Some(false),
                                    data: None,
                                });
                                continue;
                            }
                        }

                        if tok.text.starts_with('.')
                            || hctx.db.register(tok.text).is_some()
                            || hctx.db.instruction(tok.text).is_some()
                            || tok.text.bytes().next().is_some_and(|b| b.is_ascii_digit())
                        {
                            continue;
                        }

                        if let Some(info) =
                            hctx.index
                                .token_hover_info(&hctx.doc.uri, abs_start, hctx.ctx)
                            && matches!(info.kind, SymKind::Equ | SymKind::Define)
                            && let Some(val) = info.value
                        {
                            let clean_val = val.trim();
                            if !clean_val.is_empty()
                                && clean_val.len() <= MAX_VALUE_HINT_LEN
                                && clean_val != tok.text
                            {
                                hints.push(InlayHint {
                                    position: hctx
                                        .doc
                                        .line_index
                                        .offset_to_position(&hctx.doc.text, abs_end),
                                    label: InlayHintLabel::String(format!(" = {clean_val}")),
                                    kind: Some(InlayHintKind::PARAMETER),
                                    text_edits: None,
                                    tooltip: None,
                                    padding_left: Some(false),
                                    padding_right: Some(false),
                                    data: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        stmt_offset += stmt.text.len() + 1;
        let _ = stmt_offset;
    }
}

struct StrSpan<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn split_statements_with_offsets(code: &str) -> Vec<StrSpan<'_>> {
    let mut out = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut start = 0usize;

    for (idx, ch) in code.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && (in_single || in_double) {
            escaped = true;
        } else if ch == '\'' && !in_double {
            in_single = !in_single;
        } else if ch == '"' && !in_single {
            in_double = !in_double;
        } else if ch == ';' && !in_single && !in_double {
            out.push(StrSpan {
                start,
                end: idx,
                text: &code[start..idx],
            });
            start = idx + 1;
        }
    }
    if start <= code.len() {
        out.push(StrSpan {
            start,
            end: code.len(),
            text: &code[start..],
        });
    }
    out
}

fn split_top_level_with_offsets(args: &str) -> Vec<StrSpan<'_>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in args.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(StrSpan {
                    start,
                    end: idx,
                    text: &args[start..idx],
                });
                start = idx + 1;
            }
            _ => {}
        }
    }
    out.push(StrSpan {
        start,
        end: args.len(),
        text: &args[start..],
    });
    out
}

fn skip_label_prefix(stmt: &str) -> usize {
    let mut pos = 0usize;
    while let Some(colon) = stmt[pos..].find(':') {
        let label_part = &stmt[pos..pos + colon];
        if label_part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$' | b' ' | b'\t'))
        {
            pos = pos + colon + 1;
        } else {
            break;
        }
    }
    pos
}

fn extract_tokens_in_slice(slice: &str) -> Vec<StrSpan<'_>> {
    let mut tokens = Vec::new();
    let bytes = slice.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$') {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'$'))
            {
                i += 1;
            }
            tokens.push(StrSpan {
                start,
                end: i,
                text: &slice[start..i],
            });
        } else {
            i += 1;
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use lsp_types::Position;
    use std::str::FromStr;

    #[test]
    fn inlay_hints_show_csr_numbers_and_constant_values() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///hint.S").expect("uri");
        let doc = store.open(
            uri.clone(),
            1,
            ".equ MAGIC, 0x2a\n    csrr a0, stvec\n    csrw 0x300, a0\n    li a1, MAGIC\n"
                .to_owned(),
        );
        let mut ctx = CompileContext::new(None, Config::default());
        let mut idx = WorkspaceIndex::new(None).expect("idx");
        idx.upsert_open_document(doc);

        let hints = provide_inlay_hints(
            doc,
            Range::new(Position::new(0, 0), Position::new(4, 0)),
            &db,
            &mut ctx,
            &mut idx,
        )
        .expect("hints");

        let labels: Vec<String> = hints
            .iter()
            .filter_map(|h| match &h.label {
                InlayHintLabel::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&": 0x105".to_owned()), "got: {labels:?}");
        assert!(labels.contains(&": mstatus".to_owned()), "got: {labels:?}");
        assert!(labels.contains(&" = 0x2a".to_owned()), "got: {labels:?}");
    }
}
