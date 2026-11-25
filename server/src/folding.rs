use std::collections::BTreeSet;

use lsp_types::{FoldingRange, FoldingRangeKind};

use crate::document::Document;
use crate::index::{SymKind, WorkspaceIndex};
use crate::preprocessor::{HashLineKind, classify_hash_line};

pub fn provide_folding_ranges(
    doc: &Document,
    index: &WorkspaceIndex,
    preprocessed: bool,
) -> Option<Vec<FoldingRange>> {
    let mut ranges: BTreeSet<(u32, u32, u8)> = BTreeSet::new();

    collect_block_comments(&doc.text, preprocessed, &mut ranges);
    collect_directive_blocks(&doc.text, preprocessed, &mut ranges);

    if let Some(file) = index.file_by_uri(&doc.uri) {
        for def in &file.definitions {
            if def.kind == SymKind::Label {
                let r = file.range(def.full_span);
                if r.end.line > r.start.line {
                    ranges.insert((r.start.line, r.end.line, 1));
                }
            }
        }
    }

    if ranges.is_empty() {
        return None;
    }

    let out = ranges
        .into_iter()
        .map(|(start_line, end_line, kind_ord)| FoldingRange {
            start_line,
            start_character: None,
            end_line,
            end_character: None,
            kind: Some(if kind_ord == 0 {
                FoldingRangeKind::Comment
            } else {
                FoldingRangeKind::Region
            }),
            collapsed_text: None,
        })
        .collect();
    Some(out)
}

fn collect_block_comments(text: &str, preprocessed: bool, out: &mut BTreeSet<(u32, u32, u8)>) {
    let mut in_block = false;
    let mut block_start_line = 0u32;
    let mut in_single = false;
    let mut in_double = false;
    let mut line = 0u32;

    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line += 1;
            in_single = false;
            in_double = false;
            i += 1;
            continue;
        }
        if in_block {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block = false;
                if line > block_start_line {
                    out.insert((block_start_line, line, 0));
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\\' {
                i += 2.min(bytes.len() - i);
            } else {
                if b == b'\'' {
                    in_single = false;
                }
                i += 1;
            }
            continue;
        }
        if in_double {
            if b == b'\\' {
                i += 2.min(bytes.len() - i);
            } else {
                if b == b'"' {
                    in_double = false;
                }
                i += 1;
            }
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block = true;
                block_start_line = line;
                i += 2;
            }
            b'#' if !preprocessed => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    if in_block && line > block_start_line {
        out.insert((block_start_line, line, 0));
    }
}

fn collect_directive_blocks(text: &str, preprocessed: bool, out: &mut BTreeSet<(u32, u32, u8)>) {
    let mut macro_stack: Vec<u32> = Vec::new();
    let mut asm_if_stack: Vec<u32> = Vec::new();
    let mut rept_stack: Vec<u32> = Vec::new();
    let mut cpp_if_stack: Vec<u32> = Vec::new();
    let mut in_block = false;

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line_u32 = line_idx as u32;
        if preprocessed
            && !in_block
            && let Some(HashLineKind::Directive { name, .. }) = classify_hash_line(raw_line)
        {
            match name {
                "if" | "ifdef" | "ifndef" => cpp_if_stack.push(line_u32),
                "endif" => {
                    if let Some(start) = cpp_if_stack.pop()
                        && line_u32 > start
                    {
                        out.insert((start, line_u32, 1));
                    }
                }
                _ => {}
            }
        }

        let stripped = crate::line::strip_comments_with_state(raw_line, &mut in_block, false);
        for stmt in crate::line::split_unquoted_statements_stripped(&stripped) {
            let trimmed = stmt.trim();
            if !trimmed.starts_with('.') {
                continue;
            }
            let head = trimmed
                .split(|c: char| c.is_whitespace() || c == ',')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            match head.as_str() {
                ".macro" => macro_stack.push(line_u32),
                ".endm" => {
                    if let Some(start) = macro_stack.pop()
                        && line_u32 > start
                    {
                        out.insert((start, line_u32, 1));
                    }
                }
                ".if" | ".ifdef" | ".ifndef" | ".ifnotdef" | ".ifc" | ".ifnc" | ".ifeqs"
                | ".ifnes" | ".ifb" | ".ifnb" | ".ifeq" | ".ifne" | ".iflt" | ".ifle" | ".ifgt"
                | ".ifge" => asm_if_stack.push(line_u32),
                ".endif" => {
                    if let Some(start) = asm_if_stack.pop()
                        && line_u32 > start
                    {
                        out.insert((start, line_u32, 1));
                    }
                }
                ".rept" | ".irp" | ".irpc" => rept_stack.push(line_u32),
                ".endr" => {
                    if let Some(start) = rept_stack.pop()
                        && line_u32 > start
                    {
                        out.insert((start, line_u32, 1));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentStore;

    #[test]
    fn collects_macro_if_comment_and_label_folds() {
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("fold.S");
        let doc = store.open(
            uri,
            1,
            "\
/*
 * header comment
 */
.macro M a
    nop
.endm
#ifdef FOO
func:
    addi a0, a0, 1
    ret
#endif
"
            .to_owned(),
        );
        let mut idx = WorkspaceIndex::new(None).expect("idx");
        idx.upsert_open_document(doc);

        let folds = provide_folding_ranges(doc, &idx, true).expect("folds");
        assert!(folds.iter().any(|f| {
            f.start_line == 0 && f.end_line == 2 && f.kind == Some(FoldingRangeKind::Comment)
        }));
        assert!(folds.iter().any(|f| f.start_line == 3 && f.end_line == 5));
        assert!(folds.iter().any(|f| f.start_line == 6 && f.end_line == 10));
        assert!(folds.iter().any(|f| f.start_line == 7 && f.end_line == 10));
    }
}
