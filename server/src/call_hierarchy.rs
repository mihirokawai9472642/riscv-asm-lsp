use std::collections::BTreeMap;

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, Range,
    SymbolKind,
};

use crate::context::CompileContext;
use crate::index::{FileIndex, RefKind, SymKind, SymbolDef, WorkspaceIndex};
use crate::position::word_range_at;

/// Prepare a call hierarchy item for the non-local assembly label under the cursor.
pub fn prepare_call_hierarchy(
    index: &mut WorkspaceIndex,
    params: &CallHierarchyPrepareParams,
    ctx: &mut CompileContext,
) -> Option<Vec<CallHierarchyItem>> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let pre = ctx.for_uri(uri).preprocessed;
    index.sync_preprocessed(uri, pre);
    let cur_file = index.file_by_uri(uri)?.clone();
    let offset = cur_file.line_index.position_to_offset(&cur_file.text, pos);
    let (tok_start, tok_end) = word_range_at(&cur_file.text, offset);
    if tok_start == tok_end {
        return None;
    }
    let token = cur_file.text.get(tok_start..tok_end)?;
    if token == "." || token.starts_with(".L") {
        return None;
    }

    // Do not resolve `#define` / `.equ` / `.macro` scoped to this file or include closure as functions.
    let closure = index.resolve_include_closure(&cur_file.path, ctx);
    for path in &closure {
        if let Some(f) = index.file_by_path(path)
            && f.definitions.iter().any(|d| {
                matches!(d.kind, SymKind::Equ | SymKind::Macro | SymKind::Define) && d.name == token
            })
        {
            return None;
        }
    }

    let (def_file, def) = find_label_definition(index, &cur_file, token)?;
    Some(vec![build_item(def_file, def)])
}

/// Collect callers (`CallHierarchyIncomingCall`) that jump/branch/call `item.name`.
pub fn incoming_calls(
    index: &WorkspaceIndex,
    params: &CallHierarchyIncomingCallsParams,
) -> Vec<CallHierarchyIncomingCall> {
    let target = &params.item;
    let target_name = target.name.as_str();
    let target_uri_str = target.uri.as_str();

    let mut grouped: BTreeMap<(String, usize, usize), (CallHierarchyItem, Vec<Range>)> =
        BTreeMap::new();

    for f in index.files() {
        // Skip files that shadow `target.name` with a different definition unless this is `target.uri`.
        let local_shadows_other_file = f.uri.as_str() != target_uri_str
            && f.definitions
                .iter()
                .any(|d| d.kind == SymKind::Label && d.name == target_name);
        if local_shadows_other_file {
            continue;
        }

        let labels = sorted_label_defs(f);
        if labels.is_empty() {
            continue;
        }

        for r in &f.references {
            if r.kind != RefKind::Identifier || r.name != target_name {
                continue;
            }
            if !is_control_flow_statement_at(f, r.span.0) {
                continue;
            }
            let Some(caller) = enclosing_label_at(&labels, r.span.0) else {
                continue;
            };
            let call_range = f.line_index.byte_span_to_range(&f.text, r.span.0, r.span.1);
            let key = (
                f.uri.as_str().to_owned(),
                caller.name_span.0,
                caller.name_span.1,
            );
            grouped
                .entry(key)
                .or_insert_with(|| (build_item(f, caller), Vec::new()))
                .1
                .push(call_range);
        }
    }

    grouped
        .into_values()
        .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
        .collect()
}

/// Collect callees (`CallHierarchyOutgoingCall`) targeted by call/jump/branch instructions inside `item`.
pub fn outgoing_calls(
    index: &WorkspaceIndex,
    params: &CallHierarchyOutgoingCallsParams,
) -> Vec<CallHierarchyOutgoingCall> {
    let source = &params.item;
    let Some(src_file) = index.file_by_uri(&source.uri) else {
        return Vec::new();
    };
    let Some(caller_def) = src_file.definitions.iter().find(|d| {
        d.kind == SymKind::Label
            && d.name == source.name
            && src_file
                .line_index
                .byte_span_to_range(&src_file.text, d.name_span.0, d.name_span.1)
                == source.selection_range
    }) else {
        return Vec::new();
    };

    let (body_start, body_end) = caller_def.full_span;
    let mut grouped: BTreeMap<(String, usize, usize), (CallHierarchyItem, Vec<Range>)> =
        BTreeMap::new();

    for r in &src_file.references {
        if r.kind != RefKind::Identifier || r.span.0 < body_start || r.span.1 > body_end {
            continue;
        }
        if !is_control_flow_statement_at(src_file, r.span.0) {
            continue;
        }
        let Some((callee_file, callee_def)) = find_label_definition(index, src_file, &r.name)
        else {
            continue;
        };
        let call_range = src_file
            .line_index
            .byte_span_to_range(&src_file.text, r.span.0, r.span.1);
        let key = (
            callee_file.uri.as_str().to_owned(),
            callee_def.name_span.0,
            callee_def.name_span.1,
        );
        grouped
            .entry(key)
            .or_insert_with(|| (build_item(callee_file, callee_def), Vec::new()))
            .1
            .push(call_range);
    }

    grouped
        .into_values()
        .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
        .collect()
}

fn find_label_definition<'a>(
    index: &'a WorkspaceIndex,
    preferred_file: &'a FileIndex,
    name: &str,
) -> Option<(&'a FileIndex, &'a SymbolDef)> {
    if let Some(local_def) = preferred_file
        .definitions
        .iter()
        .find(|d| d.kind == SymKind::Label && d.name == name)
    {
        return Some((preferred_file, local_def));
    }

    let mut fallback: Option<(&FileIndex, &SymbolDef)> = None;
    for f in index.files() {
        if let Some(def) = f
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::Label && d.name == name)
        {
            if f.globals.contains(name) {
                return Some((f, def));
            }
            if fallback.is_none() {
                fallback = Some((f, def));
            }
        }
    }
    fallback
}

fn sorted_label_defs(file: &FileIndex) -> Vec<&SymbolDef> {
    let mut defs: Vec<&SymbolDef> = file
        .definitions
        .iter()
        .filter(|d| d.kind == SymKind::Label)
        .collect();
    defs.sort_by_key(|d| d.full_span.0);
    defs
}

fn enclosing_label_at<'a>(labels: &[&'a SymbolDef], offset: usize) -> Option<&'a SymbolDef> {
    let idx = labels.partition_point(|d| d.full_span.0 <= offset);
    let candidate = labels.get(idx.checked_sub(1)?)?;
    (offset <= candidate.full_span.1).then_some(*candidate)
}

fn build_item(file: &FileIndex, def: &SymbolDef) -> CallHierarchyItem {
    let file_name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.uri.as_str().to_owned());
    let detail = if file.globals.contains(&def.name) {
        Some(format!("global label · {file_name}"))
    } else {
        Some(format!("label · {file_name}"))
    };

    CallHierarchyItem {
        name: def.name.clone(),
        kind: SymbolKind::FUNCTION,
        tags: None,
        detail,
        uri: file.uri.clone(),
        range: file
            .line_index
            .byte_span_to_range(&file.text, def.full_span.0, def.full_span.1),
        selection_range: file.line_index.byte_span_to_range(
            &file.text,
            def.name_span.0,
            def.name_span.1,
        ),
        data: None,
    }
}

fn is_control_flow_statement_at(file: &FileIndex, ref_offset: usize) -> bool {
    let line_idx = file.line_index.line_at_offset(ref_offset);
    let (line_start, line_end, _) = file.line_index.line_bounds(&file.text, line_idx);
    let Some(line) = file.text.get(line_start..line_end) else {
        return false;
    };
    let col = ref_offset.saturating_sub(line_start);
    let Some(stmt) = crate::line::extract_statement_at(line, col) else {
        return false;
    };
    let clean = strip_leading_label_prefixes(&stmt);
    let Some(head) = clean.split_whitespace().next() else {
        return false;
    };
    is_call_or_jump_mnemonic(head)
}

fn strip_leading_label_prefixes(s: &str) -> &str {
    let mut cur = s.trim_start();
    loop {
        let bytes = cur.as_bytes();
        let mut i = 0;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'$'))
        {
            i += 1;
        }
        if i > 0 && bytes.get(i) == Some(&b':') {
            cur = cur[i + 1..].trim_start();
        } else {
            return cur;
        }
    }
}

fn is_call_or_jump_mnemonic(head: &str) -> bool {
    let lower = head.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "call"
            | "tail"
            | "jump"
            | "jal"
            | "j"
            | "jr"
            | "jalr"
            | "c.j"
            | "c.jal"
            | "c.jr"
            | "c.jalr"
            | "beq"
            | "bne"
            | "blt"
            | "bge"
            | "bltu"
            | "bgeu"
            | "beqz"
            | "bnez"
            | "blez"
            | "bgez"
            | "bltz"
            | "bgtz"
            | "bgt"
            | "ble"
            | "bgtu"
            | "bleu"
            | "c.beqz"
            | "c.bnez"
    )
}
