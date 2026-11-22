use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionResponse, Position, Range, TextEdit,
    Uri, WorkspaceEdit,
};

use crate::diagnostics::DiagnosticsCache;

pub fn provide_code_actions(
    uri: &Uri,
    range: Range,
    cache: &DiagnosticsCache,
) -> Option<CodeActionResponse> {
    let guard = cache.lock().ok()?;
    let stored = guard.get(uri.as_str())?;
    let mut actions = Vec::new();

    for item in stored {
        if item.suggestions.is_empty() || !ranges_overlap(item.diagnostic.range, range) {
            continue;
        }
        for (idx, suggestion) in item.suggestions.iter().enumerate() {
            #[allow(clippy::mutable_key_type)]
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: item.diagnostic.range,
                    new_text: suggestion.clone(),
                }],
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Replace with `{suggestion}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![item.diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: Some(idx == 0),
                ..CodeAction::default()
            }));
        }
    }

    if actions.is_empty() {
        None
    } else {
        Some(actions)
    }
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
            }],
        );
        let cache = Arc::new(Mutex::new(map));
        let actions = provide_code_actions(
            &uri,
            Range::new(Position::new(4, 5), Position::new(4, 5)),
            &cache,
        )
        .expect("actions");
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(ca) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(ca.title, "Replace with `mv`");
        assert_eq!(ca.kind, Some(CodeActionKind::QUICKFIX));
    }
}
