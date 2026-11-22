use std::collections::{HashMap, HashSet};

use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, GotoDefinitionResponse, Location, Position,
    PrepareRenameResponse, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::context::{CompileContext, path_to_uri};
use crate::data::Database;
use crate::index::{
    FileIndex, ParamScope, RefKind, SymKind, SymbolDef, WorkspaceIndex, parse_macro_var_token,
    parse_numeric_ref_token, resolve_numeric_def, resolve_param_scope, visible_params_at,
};
use crate::position::word_range_at;

#[derive(Debug, Clone)]
enum TargetSymbol {
    Numeric {
        digits: String,
        def_span: (usize, usize),
    },
    MacroParam {
        scope: ParamScope,
        param_name: String,
    },
    LocalLabel {
        name: String,
    },
    ScopedEquOrMacro {
        name: String,
        def_file: FileIndex,
    },
    Define {
        name: String,
        def_file: FileIndex,
    },
    GlobalLabel {
        name: String,
    },
}

impl TargetSymbol {
    fn current_name(&self) -> Option<&str> {
        match self {
            TargetSymbol::Numeric { .. } => None,
            TargetSymbol::MacroParam { param_name, .. } => Some(param_name.as_str()),
            TargetSymbol::LocalLabel { name }
            | TargetSymbol::ScopedEquOrMacro { name, .. }
            | TargetSymbol::Define { name, .. }
            | TargetSymbol::GlobalLabel { name } => Some(name.as_str()),
        }
    }
}

pub fn goto_definition(
    index: &mut WorkspaceIndex,
    uri: &Uri,
    pos: Position,
    ctx: &mut CompileContext,
) -> Option<GotoDefinitionResponse> {
    if let Some((_, resolved_path)) = index.resolve_include_on_line(uri, pos.line, ctx)
        && let Some(target_uri) = path_to_uri(&resolved_path)
    {
        return Some(GotoDefinitionResponse::Scalar(Location::new(
            target_uri,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
        )));
    }

    let file = index.file_by_uri(uri)?.clone();
    let offset = file.line_index.position_to_offset(&file.text, pos);
    let target = identify_target(index, &file, offset, ctx)?;

    let mut locations = Vec::new();
    match target {
        TargetSymbol::Numeric { def_span, .. } => {
            locations.push(file.location(def_span));
        }
        TargetSymbol::MacroParam { scope, param_name } => match scope {
            ParamScope::Macro(macro_idx) => {
                if let Some(macro_def) = file.definitions.get(macro_idx)
                    && let Some(p) = macro_def.params.iter().find(|p| p.name == param_name)
                {
                    locations.push(file.location(p.span));
                }
            }
            ParamScope::Repeat(rep_idx) => {
                if let Some(rep) = file.repeat_scopes.get(rep_idx)
                    && rep.param.name == param_name
                {
                    locations.push(file.location(rep.param.span));
                }
            }
        },
        TargetSymbol::LocalLabel { name } => {
            for def in &file.definitions {
                if def.kind == SymKind::LocalLabel && def.name == name {
                    locations.push(file.location(def.name_span));
                }
            }
        }
        TargetSymbol::ScopedEquOrMacro { name, .. } => {
            let closure = index.resolve_include_closure(&file.path, ctx);
            for path in closure {
                if let Some(f) = index.files().find(|f| f.path == path) {
                    for def in &f.definitions {
                        if matches!(d_kind(def), SymKind::Equ | SymKind::Macro) && def.name == name
                        {
                            locations.push(f.location(def.name_span));
                        }
                    }
                }
            }
        }
        TargetSymbol::Define { name, .. } => {
            let closure = index.resolve_include_closure(&file.path, ctx);
            for path in closure {
                if let Some(f) = index.files().find(|f| f.path == path) {
                    for def in &f.definitions {
                        if def.kind == SymKind::Define && def.name == name {
                            locations.push(f.location(def.name_span));
                        }
                    }
                }
            }
        }
        TargetSymbol::GlobalLabel { name } => {
            for f in index.files() {
                for def in &f.definitions {
                    if def.kind == SymKind::Label && def.name == name {
                        locations.push(f.location(def.name_span));
                    }
                }
            }
        }
    }

    if locations.is_empty() {
        None
    } else if locations.len() == 1 {
        Some(GotoDefinitionResponse::Scalar(locations.remove(0)))
    } else {
        Some(GotoDefinitionResponse::Array(locations))
    }
}

pub fn find_references(
    index: &mut WorkspaceIndex,
    uri: &Uri,
    pos: Position,
    include_declaration: bool,
    ctx: &mut CompileContext,
) -> Option<Vec<Location>> {
    let file = index.file_by_uri(uri)?.clone();
    let offset = file.line_index.position_to_offset(&file.text, pos);
    let target = identify_target(index, &file, offset, ctx)?;
    let locs = collect_occurrences(index, &file, &target, include_declaration, false, ctx);
    if locs.is_empty() { None } else { Some(locs) }
}

pub fn document_highlight(
    index: &mut WorkspaceIndex,
    uri: &Uri,
    pos: Position,
    ctx: &mut CompileContext,
) -> Option<Vec<DocumentHighlight>> {
    let file = index.file_by_uri(uri)?.clone();
    let offset = file.line_index.position_to_offset(&file.text, pos);
    let target = identify_target(index, &file, offset, ctx)?;
    let locs = collect_occurrences(index, &file, &target, true, true, ctx);
    if locs.is_empty() {
        return None;
    }
    Some(
        locs.into_iter()
            .map(|l| DocumentHighlight {
                range: l.range,
                kind: Some(DocumentHighlightKind::TEXT),
            })
            .collect(),
    )
}

pub fn prepare_rename(
    index: &mut WorkspaceIndex,
    uri: &Uri,
    pos: Position,
    db: &Database,
    ctx: &mut CompileContext,
) -> Option<PrepareRenameResponse> {
    let file = index.file_by_uri(uri)?.clone();
    let offset = file.line_index.position_to_offset(&file.text, pos);
    let (tok_start, tok_end) = word_range_at(&file.text, offset);
    if tok_start == tok_end {
        return None;
    }
    let token = file.text.get(tok_start..tok_end)?;

    // Refuse built-in mnemonics, registers, CSRs, directives
    if db.instruction(token).is_some()
        || db.register(token).is_some()
        || db.csr(token).is_some()
        || db.directive(token).is_some()
    {
        return None;
    }

    let target = identify_target(index, &file, offset, ctx)?;
    if !is_renameable(index, &file, &target, ctx) {
        return None;
    }

    // For `\param`, keep only `param` editable inside the range (exclude leading `\`)
    let edit_span = match &target {
        TargetSymbol::MacroParam { .. } => {
            if token.starts_with('\\') {
                (tok_start + 1, tok_end)
            } else {
                (tok_start, tok_end)
            }
        }
        _ => (tok_start, tok_end),
    };
    let range = file.range(edit_span);
    Some(PrepareRenameResponse::Range(range))
}

pub fn rename_symbol(
    index: &mut WorkspaceIndex,
    uri: &Uri,
    pos: Position,
    new_name: &str,
    db: &Database,
    ctx: &mut CompileContext,
) -> Result<Option<WorkspaceEdit>, String> {
    let Some(file) = index.file_by_uri(uri).cloned() else {
        return Ok(None);
    };
    let offset = file.line_index.position_to_offset(&file.text, pos);
    let (tok_start, tok_end) = word_range_at(&file.text, offset);
    let Some(token) = file.text.get(tok_start..tok_end) else {
        return Ok(None);
    };
    if db.instruction(token).is_some()
        || db.register(token).is_some()
        || db.csr(token).is_some()
        || db.directive(token).is_some()
    {
        return Ok(None);
    }

    let Some(target) = identify_target(index, &file, offset, ctx) else {
        return Ok(None);
    };
    if !is_renameable(index, &file, &target, ctx) {
        return Ok(None);
    }

    validate_rename_target(index, &file, &target, new_name, db, ctx)?;
    if target.current_name() == Some(new_name) {
        return Ok(None);
    }

    let locs = collect_occurrences(index, &file, &target, true, false, ctx);
    if locs.is_empty() {
        return Ok(None);
    }

    let is_macro_param = matches!(target, TargetSymbol::MacroParam { .. });
    #[allow(clippy::mutable_key_type)]
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for loc in locs {
        let mut range = loc.range;
        if is_macro_param {
            // Adjust reference `\param` span so leading `\` stays intact
            if let Some(target_file) = index.file_by_uri(&loc.uri) {
                let s = target_file
                    .line_index
                    .position_to_offset(&target_file.text, range.start);
                if target_file.text.get(s..s + 1) == Some("\\") {
                    range.start = target_file
                        .line_index
                        .offset_to_position(&target_file.text, s + 1);
                }
            }
        }
        changes.entry(loc.uri).or_default().push(TextEdit {
            range,
            new_text: new_name.to_owned(),
        });
    }

    Ok(Some(WorkspaceEdit {
        changes: Some(changes),
        ..WorkspaceEdit::default()
    }))
}

fn validate_rename_target(
    index: &mut WorkspaceIndex,
    file: &FileIndex,
    target: &TargetSymbol,
    new_name: &str,
    db: &Database,
    ctx: &mut CompileContext,
) -> Result<(), String> {
    if !is_valid_symbol_name(new_name) {
        return Err(format!("invalid symbol name `{new_name}`"));
    }
    if db.register(new_name).is_some() {
        return Err(format!(
            "cannot rename symbol to register name `{new_name}`"
        ));
    }
    if db.csr(new_name).is_some() {
        return Err(format!("cannot rename symbol to CSR name `{new_name}`"));
    }
    if new_name.starts_with('.') && db.directive(new_name).is_some() {
        return Err(format!(
            "cannot rename symbol to assembler directive `{new_name}`"
        ));
    }
    if target.current_name() == Some(new_name) {
        return Ok(());
    }

    let collides = match target {
        TargetSymbol::Numeric { .. } => false,
        TargetSymbol::MacroParam { scope, .. } => match *scope {
            ParamScope::Macro(macro_idx) => file
                .definitions
                .get(macro_idx)
                .is_some_and(|m| m.params.iter().any(|p| p.name == new_name)),
            ParamScope::Repeat(rep_idx) => file.repeat_scopes.get(rep_idx).is_some_and(|rep| {
                visible_params_at(file, rep.param.span.0)
                    .iter()
                    .any(|p| p.name == new_name)
            }),
        },
        TargetSymbol::LocalLabel { .. } => file.definitions.iter().any(|d| {
            !matches!(d.kind, SymKind::NumericLabel | SymKind::MacroParam) && d.name == new_name
        }),
        TargetSymbol::ScopedEquOrMacro { def_file, .. } | TargetSymbol::Define { def_file, .. } => {
            let mut scope_files: HashSet<std::path::PathBuf> = index
                .resolve_include_closure(&file.path, ctx)
                .into_iter()
                .collect();
            for p in index.files_including(&def_file.path, ctx) {
                scope_files.insert(p);
            }
            scope_files.iter().any(|path| {
                index.files().find(|f| f.path == *path).is_some_and(|f| {
                    f.definitions.iter().any(|d| {
                        !matches!(d.kind, SymKind::NumericLabel | SymKind::MacroParam)
                            && d.name == new_name
                    })
                })
            })
        }
        TargetSymbol::GlobalLabel { name: cur_name } => {
            let global_collision = index.files().any(|f| {
                f.definitions
                    .iter()
                    .any(|d| d.kind == SymKind::Label && d.name == new_name)
            });
            let same_file_collision = index.files().any(|f| {
                let defines_cur = f.path == file.path
                    || f.definitions
                        .iter()
                        .any(|d| d.kind == SymKind::Label && d.name == *cur_name);
                defines_cur
                    && f.definitions.iter().any(|d| {
                        !matches!(d.kind, SymKind::NumericLabel | SymKind::MacroParam)
                            && d.name == new_name
                    })
            });
            global_collision || same_file_collision
        }
    };

    if collides {
        return Err(format!(
            "symbol `{new_name}` is already defined in this scope"
        ));
    }
    Ok(())
}

fn is_valid_symbol_name(name: &str) -> bool {
    if name.is_empty() || name == "." {
        return false;
    }
    let bytes = name.as_bytes();
    let first_ok = matches!(bytes[0], b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'.' | b'$');
    first_ok
        && bytes[1..]
            .iter()
            .all(|&b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'$'))
}

fn is_renameable(
    index: &mut WorkspaceIndex,
    cur_file: &FileIndex,
    target: &TargetSymbol,
    _ctx: &mut CompileContext,
) -> bool {
    match target {
        TargetSymbol::Numeric { .. } => false,
        TargetSymbol::MacroParam { .. } | TargetSymbol::LocalLabel { .. } => {
            index.is_inside_workspace(&cur_file.path)
        }
        TargetSymbol::ScopedEquOrMacro { def_file, .. } | TargetSymbol::Define { def_file, .. } => {
            index.is_inside_workspace(&def_file.path)
        }
        TargetSymbol::GlobalLabel { name } => {
            let mut has_any_def = false;
            for f in index.files() {
                if f.definitions
                    .iter()
                    .any(|d| d.kind == SymKind::Label && d.name == *name)
                {
                    has_any_def = true;
                    if !index.is_inside_workspace(&f.path) {
                        return false;
                    }
                }
            }
            has_any_def
        }
    }
}

fn identify_target(
    index: &mut WorkspaceIndex,
    file: &FileIndex,
    offset: usize,
    ctx: &mut CompileContext,
) -> Option<TargetSymbol> {
    // 1. Check if cursor is directly on a numeric label definition (`1:`)
    if let Some(def) = file.definitions.iter().find(|d| {
        d.kind == SymKind::NumericLabel && offset >= d.name_span.0 && offset <= d.full_span.1
    }) {
        return Some(TargetSymbol::Numeric {
            digits: def.name.clone(),
            def_span: def.name_span,
        });
    }

    // 2. Check if cursor is on a macro or `.irp` / `.irpc` parameter declaration
    if let Some((rep_idx, rep)) = file
        .repeat_scopes
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| offset >= s.param.span.0 && offset <= s.param.span.1)
    {
        return Some(TargetSymbol::MacroParam {
            scope: ParamScope::Repeat(rep_idx),
            param_name: rep.param.name.clone(),
        });
    }
    if let Some((macro_idx, param)) = file.definitions.iter().enumerate().find_map(|(i, d)| {
        if d.kind == SymKind::Macro {
            d.params
                .iter()
                .find(|p| offset >= p.span.0 && offset <= p.span.1)
                .map(|p| (i, p.name.clone()))
        } else {
            None
        }
    }) {
        return Some(TargetSymbol::MacroParam {
            scope: ParamScope::Macro(macro_idx),
            param_name: param,
        });
    }

    let (tok_start, tok_end) = word_range_at(&file.text, offset);
    if tok_start == tok_end {
        return None;
    }
    let token = file.text.get(tok_start..tok_end)?;

    // 3. Numeric label reference (`1f` / `1b`)
    if let Some((digits, forward)) = parse_numeric_ref_token(token) {
        let def = resolve_numeric_def(file, digits, forward, tok_start)?;
        return Some(TargetSymbol::Numeric {
            digits: digits.to_owned(),
            def_span: def.name_span,
        });
    }

    // 4. Macro or `.irp` / `.irpc` parameter reference (`\reg`)
    if let Some(param_name) = parse_macro_var_token(token)
        && let Some((scope, p, ..)) = resolve_param_scope(file, tok_start, param_name)
    {
        return Some(TargetSymbol::MacroParam {
            scope,
            param_name: p.name.clone(),
        });
    }

    // 5. File-scoped local label (`.L...`)
    if token.starts_with(".L")
        && file
            .definitions
            .iter()
            .any(|d| d.kind == SymKind::LocalLabel && d.name == token)
    {
        return Some(TargetSymbol::LocalLabel {
            name: token.to_owned(),
        });
    }

    // 6. Check current file & include closure for `.equ`, `.macro`, `#define`
    let closure = index.resolve_include_closure(&file.path, ctx);
    for path in &closure {
        if let Some(f) = index.files().find(|f| f.path == *path)
            && let Some(def) = f.definitions.iter().find(|d| {
                matches!(d.kind, SymKind::Equ | SymKind::Macro | SymKind::Define) && d.name == token
            })
        {
            return match def.kind {
                SymKind::Define => Some(TargetSymbol::Define {
                    name: token.to_owned(),
                    def_file: f.clone(),
                }),
                _ => Some(TargetSymbol::ScopedEquOrMacro {
                    name: token.to_owned(),
                    def_file: f.clone(),
                }),
            };
        }
    }

    // 7. Project-wide label (ordinary labels or references to them)
    if index.files().any(|f| {
        f.definitions
            .iter()
            .any(|d| d.kind == SymKind::Label && d.name == token)
    }) {
        return Some(TargetSymbol::GlobalLabel {
            name: token.to_owned(),
        });
    }

    None
}

fn collect_occurrences(
    index: &mut WorkspaceIndex,
    cur_file: &FileIndex,
    target: &TargetSymbol,
    include_declaration: bool,
    current_file_only: bool,
    ctx: &mut CompileContext,
) -> Vec<Location> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    match target {
        TargetSymbol::Numeric { digits, def_span } => {
            if include_declaration {
                push_unique(&mut out, &mut seen, cur_file.location(*def_span));
            }
            for r in &cur_file.references {
                let forward = match r.kind {
                    RefKind::NumericForward => true,
                    RefKind::NumericBackward => false,
                    _ => continue,
                };
                if r.name == *digits
                    && let Some(resolved) = resolve_numeric_def(cur_file, digits, forward, r.span.0)
                    && resolved.name_span == *def_span
                {
                    push_unique(&mut out, &mut seen, cur_file.location(r.span));
                }
            }
        }
        TargetSymbol::MacroParam { scope, param_name } => {
            if include_declaration {
                match scope {
                    ParamScope::Macro(macro_idx) => {
                        if let Some(macro_def) = cur_file.definitions.get(*macro_idx)
                            && let Some(p) = macro_def.params.iter().find(|p| p.name == *param_name)
                        {
                            push_unique(&mut out, &mut seen, cur_file.location(p.span));
                        }
                    }
                    ParamScope::Repeat(rep_idx) => {
                        if let Some(rep) = cur_file.repeat_scopes.get(*rep_idx)
                            && rep.param.name == *param_name
                        {
                            push_unique(&mut out, &mut seen, cur_file.location(rep.param.span));
                        }
                    }
                }
            }
            for r in &cur_file.references {
                if r.kind == RefKind::MacroParam
                    && r.name == *param_name
                    && resolve_param_scope(cur_file, r.span.0, &r.name)
                        .is_some_and(|(resolved_scope, ..)| resolved_scope == *scope)
                {
                    push_unique(&mut out, &mut seen, cur_file.location(r.span));
                }
            }
        }
        TargetSymbol::LocalLabel { name } => {
            if include_declaration {
                for def in &cur_file.definitions {
                    if def.kind == SymKind::LocalLabel && def.name == *name {
                        push_unique(&mut out, &mut seen, cur_file.location(def.name_span));
                    }
                }
            }
            for r in &cur_file.references {
                if r.kind == RefKind::Identifier && r.name == *name {
                    push_unique(&mut out, &mut seen, cur_file.location(r.span));
                }
            }
        }
        TargetSymbol::ScopedEquOrMacro { name, def_file }
        | TargetSymbol::Define { name, def_file } => {
            let scope_files = if current_file_only {
                vec![cur_file.path.clone()]
            } else {
                let mut set: HashSet<_> = index
                    .resolve_include_closure(&cur_file.path, ctx)
                    .into_iter()
                    .collect();
                for p in index.files_including(&def_file.path, ctx) {
                    set.insert(p);
                }
                set.into_iter().collect()
            };
            let is_define = matches!(target, TargetSymbol::Define { .. });

            for path in scope_files {
                if let Some(f) = index.files().find(|f| f.path == path) {
                    if include_declaration {
                        for def in &f.definitions {
                            let match_kind = if is_define {
                                def.kind == SymKind::Define
                            } else {
                                matches!(def.kind, SymKind::Equ | SymKind::Macro)
                            };
                            if match_kind && def.name == *name {
                                push_unique(&mut out, &mut seen, f.location(def.name_span));
                            }
                        }
                    }
                    for r in &f.references {
                        if r.kind == RefKind::Identifier && r.name == *name {
                            push_unique(&mut out, &mut seen, f.location(r.span));
                        }
                    }
                }
            }
        }
        TargetSymbol::GlobalLabel { name } => {
            let files: Vec<FileIndex> = if current_file_only {
                vec![cur_file.clone()]
            } else {
                index.files().cloned().collect()
            };
            for f in files {
                if include_declaration {
                    for def in &f.definitions {
                        if def.kind == SymKind::Label && def.name == *name {
                            push_unique(&mut out, &mut seen, f.location(def.name_span));
                        }
                    }
                }
                for r in &f.references {
                    if r.kind == RefKind::Identifier && r.name == *name {
                        push_unique(&mut out, &mut seen, f.location(r.span));
                    }
                }
            }
        }
    }

    out
}

fn push_unique(out: &mut Vec<Location>, seen: &mut HashSet<(String, u32, u32)>, loc: Location) {
    let key = (
        loc.uri.as_str().to_owned(),
        loc.range.start.line,
        loc.range.start.character,
    );
    if seen.insert(key) {
        out.push(loc);
    }
}

fn d_kind(def: &SymbolDef) -> SymKind {
    def.kind
}
