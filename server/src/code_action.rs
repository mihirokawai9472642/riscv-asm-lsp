use std::collections::{HashMap, HashSet};

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionResponse, Diagnostic, Position,
    Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::diagnostics::{DiagnosticsCache, StoredDiagnostic};
use crate::document::Document;
use crate::preprocessor::{classify_hash_line, hash_to_slash_replacement};

fn single_file_edit(uri: &Uri, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(std::iter::once((uri.clone(), edits)).collect()),
        ..WorkspaceEdit::default()
    }
}

pub fn provide_code_actions(
    uri: &Uri,
    range: Range,
    doc: Option<&Document>,
    preprocessed: bool,
    cache: &DiagnosticsCache,
) -> Option<CodeActionResponse> {
    let stored: Vec<StoredDiagnostic> = cache
        .lock()
        .ok()
        .and_then(|guard| guard.get(uri.as_str()).cloned())
        .unwrap_or_default();
    let mut actions = Vec::new();
    let mut hash_diags_by_line: HashMap<u32, Vec<Diagnostic>> = HashMap::new();
    let mut hash_diag_lines = Vec::new();
    let mut seen_hint_lines = HashSet::new();

    for item in &stored {
        if !ranges_overlap(item.diagnostic.range, range) {
            continue;
        }
        push_suggestion_actions(uri, item, &mut actions);
        if preprocessed && let Some(document) = doc {
            collect_hash_hints_for_item(
                uri,
                document,
                item,
                &mut hash_diags_by_line,
                &mut hash_diag_lines,
                &mut seen_hint_lines,
                &mut actions,
            );
        }
    }

    if preprocessed && let Some(document) = doc {
        push_hash_comment_actions(
            uri,
            document,
            range,
            hash_diag_lines,
            &mut hash_diags_by_line,
            &mut actions,
        );
    }

    (!actions.is_empty()).then_some(actions)
}

fn push_suggestion_actions(
    uri: &Uri,
    item: &StoredDiagnostic,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    for (idx, suggestion) in item.suggestions.iter().enumerate() {
        let edit = TextEdit {
            range: item.diagnostic.range,
            new_text: suggestion.clone(),
        };
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Replace with `{suggestion}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![item.diagnostic.clone()]),
            edit: Some(single_file_edit(uri, vec![edit])),
            is_preferred: Some(idx == 0),
            ..CodeAction::default()
        }));
    }
}

fn collect_hash_hints_for_item(
    uri: &Uri,
    document: &Document,
    item: &StoredDiagnostic,
    hash_diags_by_line: &mut HashMap<u32, Vec<Diagnostic>>,
    hash_diag_lines: &mut Vec<u32>,
    seen_hint_lines: &mut HashSet<u32>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    let diag_line = item.diagnostic.range.start.line;
    if is_hash_line(document, diag_line as usize) {
        hash_diags_by_line
            .entry(diag_line)
            .or_insert_with(|| {
                hash_diag_lines.push(diag_line);
                Vec::new()
            })
            .push(item.diagnostic.clone());
    }

    for hint in &item.macro_def_hints {
        if seen_hint_lines.insert(hint.def_line)
            && let Some(edit) = make_hash_to_slash_edit(document, hint.def_line as usize)
        {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!(
                    "Use `//` comment on line {} (defines `{}`)",
                    hint.def_line + 1,
                    hint.macro_name
                ),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![item.diagnostic.clone()]),
                edit: Some(single_file_edit(uri, vec![edit])),
                is_preferred: Some(false),
                ..CodeAction::default()
            }));
        }
    }
}

fn push_hash_comment_actions(
    uri: &Uri,
    document: &Document,
    range: Range,
    hash_diag_lines: Vec<u32>,
    hash_diags_by_line: &mut HashMap<u32, Vec<Diagnostic>>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    let mut quickfixed_lines = HashSet::new();
    for line in hash_diag_lines {
        if let Some(edit) = make_hash_to_slash_edit(document, line as usize)
            && let Some(diags) = hash_diags_by_line.remove(&line)
        {
            quickfixed_lines.insert(line);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Use `//` comment".to_owned(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(diags),
                edit: Some(single_file_edit(uri, vec![edit])),
                is_preferred: Some(true),
                ..CodeAction::default()
            }));
        }
    }

    let cursor_line = range.start.line;
    if !quickfixed_lines.contains(&cursor_line)
        && is_hash_comment_line(document, cursor_line as usize)
        && let Some(edit) = make_hash_to_slash_edit(document, cursor_line as usize)
    {
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: "Use `//` comment".to_owned(),
            kind: Some(CodeActionKind::REFACTOR_REWRITE),
            diagnostics: None,
            edit: Some(single_file_edit(uri, vec![edit])),
            ..CodeAction::default()
        }));
    }
}

fn is_hash_line(doc: &Document, line_0based: usize) -> bool {
    let (ls, ce, _) = doc.line_index.line_bounds(&doc.text, line_0based);
    doc.text.get(ls..ce).and_then(classify_hash_line).is_some()
}

fn is_hash_comment_line(doc: &Document, line_0based: usize) -> bool {
    let (ls, ce, _) = doc.line_index.line_bounds(&doc.text, line_0based);
    matches!(
        doc.text.get(ls..ce).and_then(classify_hash_line),
        Some(crate::preprocessor::HashLineKind::Comment { .. })
    )
}

fn make_hash_to_slash_edit(doc: &Document, line_0based: usize) -> Option<TextEdit> {
    let (ls, ce, _) = doc.line_index.line_bounds(&doc.text, line_0based);
    let line = doc.text.get(ls..ce)?;
    let kind = classify_hash_line(line)?;
    let hash_off = kind.hash_offset();
    let hash_byte = ls + hash_off;
    let new_text = hash_to_slash_replacement(line, hash_off).to_owned();
    Some(TextEdit {
        range: doc
            .line_index
            .byte_span_to_range(&doc.text, hash_byte, hash_byte + 1),
        new_text,
    })
}

fn ranges_overlap(a: Range, b: Range) -> bool {
    pos_le(a.start, b.end) && pos_le(b.start, a.end)
}

fn pos_le(a: Position, b: Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::StoredDiagnostic;
    use lsp_types::{Diagnostic, DiagnosticSeverity};
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    #[test]
    fn produces_quickfix_for_overlapping_diagnostic() {
        let uri = Uri::from_str("file:///test.S").expect("uri");
        let diag = Diagnostic {
            range: Range::new(Position::new(4, 4), Position::new(4, 7)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("clang".to_owned()),
            message: "unrecognized instruction mnemonic, did you mean: mv?".to_owned(),
            ..Diagnostic::default()
        };
        let mut map = HashMap::new();
        map.insert(
            uri.as_str().to_owned(),
            vec![StoredDiagnostic {
                diagnostic: diag,
                suggestions: vec!["mv".to_owned()],
                macro_def_hints: Vec::new(),
            }],
        );
        let cache = Arc::new(Mutex::new(map));
        let actions = provide_code_actions(
            &uri,
            Range::new(Position::new(4, 5), Position::new(4, 5)),
            None,
            true,
            &cache,
        )
        .expect("actions");
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(ca) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(ca.title, "Replace with `mv`");
        assert_eq!(ca.kind, Some(CodeActionKind::QUICKFIX));
        // Valid `#include` / `#define` without diagnostics do not emit REFACTOR_REWRITE; `# comment` still does.
        let mut doc_store = crate::document::DocumentStore::new().expect("store");
        let pp_uri = Uri::from_str("file:///valid_only.S").expect("uri");
        let pp_doc = doc_store.open(
            pp_uri.clone(),
            1,
            "#include <regs.h>\n#define OK 1\n# pure comment\n".to_owned(),
        );
        assert!(
            provide_code_actions(
                &pp_uri,
                Range::new(Position::new(0, 0), Position::new(0, 0)),
                Some(pp_doc),
                true,
                &cache,
            )
            .is_none()
        );
        assert!(
            provide_code_actions(
                &pp_uri,
                Range::new(Position::new(1, 0), Position::new(1, 0)),
                Some(pp_doc),
                true,
                &cache,
            )
            .is_none()
        );
        assert!(
            provide_code_actions(
                &pp_uri,
                Range::new(Position::new(2, 0), Position::new(2, 0)),
                Some(pp_doc),
                true,
                &cache,
            )
            .is_some()
        );
    }

    #[test]
    fn converts_hash_comments_and_macro_def_hints_only_when_preprocessed() {
        use crate::diagnostics::MacroDefHint;
        use crate::document::DocumentStore;

        let mut store = DocumentStore::new().expect("store");
        let uri = Uri::from_str("file:///pp.S").expect("uri");
        let doc = store.open(
            uri.clone(),
            1,
            "# just a comment\n#define X 5 is the size\n    #include this\n    li a0, X\n"
                .to_owned(),
        );
        let mut map = HashMap::new();
        map.insert(
            uri.as_str().to_owned(),
            vec![
                StoredDiagnostic {
                    diagnostic: Diagnostic {
                        range: Range::new(Position::new(2, 13), Position::new(2, 17)),
                        severity: Some(DiagnosticSeverity::ERROR),
                        ..Diagnostic::default()
                    },
                    suggestions: Vec::new(),
                    macro_def_hints: Vec::new(),
                },
                StoredDiagnostic {
                    diagnostic: Diagnostic {
                        range: Range::new(Position::new(3, 11), Position::new(3, 12)),
                        severity: Some(DiagnosticSeverity::ERROR),
                        ..Diagnostic::default()
                    },
                    suggestions: Vec::new(),
                    macro_def_hints: vec![MacroDefHint {
                        macro_name: "X".to_owned(),
                        def_line: 1,
                    }],
                },
            ],
        );
        let cache = Arc::new(Mutex::new(map));

        // Diagnostic on `#include this` yields QUICKFIX `Use // comment`
        let a_inc = provide_code_actions(
            &uri,
            Range::new(Position::new(2, 14), Position::new(2, 14)),
            Some(doc),
            true,
            &cache,
        )
        .expect("inc quickfix");
        assert!(matches!(
            &a_inc[0],
            CodeActionOrCommand::CodeAction(ca)
                if ca.title == "Use `//` comment" && ca.kind == Some(CodeActionKind::QUICKFIX)
        ));

        // Diagnostic on `li a0, X` yields QUICKFIX targeting line 1 (`#define X ...` -> `// define X ...`)
        let a_hint = provide_code_actions(
            &uri,
            Range::new(Position::new(3, 11), Position::new(3, 11)),
            Some(doc),
            true,
            &cache,
        )
        .expect("hint quickfix");
        let CodeActionOrCommand::CodeAction(ca_hint) = &a_hint[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(ca_hint.kind, Some(CodeActionKind::QUICKFIX));
        let edits = ca_hint
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .get(&uri)
            .unwrap();
        assert_eq!(edits[0].new_text, "// ");
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(1, 0), Position::new(1, 1))
        );

        // Non-preprocessed file emits no hash-comment rewrite actions
        assert!(
            provide_code_actions(
                &uri,
                Range::new(Position::new(0, 0), Position::new(0, 0)),
                Some(doc),
                false,
                &cache,
            )
            .is_none()
        );
        // Pure `# just a comment` still emits REFACTOR_REWRITE
        let actions_on_comment = provide_code_actions(
            &uri,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            Some(doc),
            true,
            &cache,
        )
        .expect("refactor rewrite on # comment");
        assert!(matches!(
            &actions_on_comment[0],
            CodeActionOrCommand::CodeAction(ca)
                if ca.kind == Some(CodeActionKind::REFACTOR_REWRITE)
        ));
    }
}
