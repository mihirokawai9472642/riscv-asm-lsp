use lsp_types::{DocumentSymbol, DocumentSymbolResponse, SymbolInformation, SymbolKind, Uri};

use crate::index::{SymKind, WorkspaceIndex};

#[allow(deprecated)]
pub fn document_symbols(index: &WorkspaceIndex, uri: &Uri) -> Option<DocumentSymbolResponse> {
    let file = index.file_by_uri(uri)?;
    let mut top_level: Vec<DocumentSymbol> = Vec::new();
    let mut label_children: Vec<Vec<DocumentSymbol>> = vec![Vec::new(); file.definitions.len()];

    for def in &file.definitions {
        if def.kind == SymKind::NumericLabel
            && let Some(parent_idx) = def.enclosing_label_idx
        {
            let name_range = file.range(def.name_span);
            let sym = DocumentSymbol {
                name: format!("{}:", def.name),
                detail: None,
                kind: SymbolKind::NUMBER,
                tags: None,
                deprecated: None,
                range: file.range(def.full_span),
                selection_range: name_range,
                children: None,
            };
            if let Some(bucket) = label_children.get_mut(parent_idx) {
                bucket.push(sym);
            }
        }
    }

    for (idx, def) in file.definitions.iter().enumerate() {
        let (kind, detail, children) = match def.kind {
            SymKind::Label => (
                SymbolKind::FUNCTION,
                file.globals
                    .contains(&def.name)
                    .then(|| "global".to_owned()),
                {
                    let ch = std::mem::take(&mut label_children[idx]);
                    if ch.is_empty() { None } else { Some(ch) }
                },
            ),
            SymKind::LocalLabel => (SymbolKind::KEY, Some("local".to_owned()), None),
            SymKind::NumericLabel => {
                if def.enclosing_label_idx.is_some() {
                    continue;
                }
                (SymbolKind::NUMBER, None, None)
            }
            SymKind::Equ => (SymbolKind::CONSTANT, def.value_text.clone(), None),
            SymKind::Macro => {
                let param_list = def
                    .params
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    SymbolKind::METHOD,
                    (!param_list.is_empty()).then_some(param_list),
                    None,
                )
            }
            SymKind::Define => (SymbolKind::CONSTANT, def.value_text.clone(), None),
            SymKind::MacroParam => continue,
        };

        let range = file.range(def.full_span);
        let selection_range = file.range(def.name_span);
        let name = if def.kind == SymKind::NumericLabel {
            format!("{}:", def.name)
        } else {
            def.name.clone()
        };

        top_level.push(DocumentSymbol {
            name,
            detail,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children,
        });
    }

    top_level.sort_by_key(|s| {
        (
            s.selection_range.start.line,
            s.selection_range.start.character,
        )
    });
    Some(DocumentSymbolResponse::Nested(top_level))
}

#[allow(deprecated)]
pub fn workspace_symbols(index: &WorkspaceIndex, query: &str) -> Vec<SymbolInformation> {
    let q = query.trim().to_ascii_lowercase();
    let mut out = Vec::new();

    for file in index.files() {
        for def in &file.definitions {
            if matches!(def.kind, SymKind::NumericLabel | SymKind::MacroParam) {
                continue;
            }
            if !q.is_empty() && !def.name.to_ascii_lowercase().contains(&q) {
                continue;
            }
            let kind = match def.kind {
                SymKind::Label => SymbolKind::FUNCTION,
                SymKind::LocalLabel => SymbolKind::KEY,
                SymKind::Equ | SymKind::Define => SymbolKind::CONSTANT,
                SymKind::Macro => SymbolKind::METHOD,
                SymKind::NumericLabel | SymKind::MacroParam => continue,
            };
            out.push(SymbolInformation {
                name: def.name.clone(),
                kind,
                tags: None,
                deprecated: None,
                location: file.location(def.name_span),
                container_name: file
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_owned),
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
