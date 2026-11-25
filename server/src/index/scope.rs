use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use lsp_types::{Range, Uri};

use crate::context::{CompileContext, path_key, path_to_uri};
use crate::position::word_range_at;

use super::parse::{parse_asm_include_line, parse_cpp_include_line, resolve_include_path};
use super::{
    CompletionSymbol, FileIndex, MacroParamDef, ParamScope, SymKind, SymbolDef, SymbolHoverInfo,
    VisibleCompletions, WorkspaceIndex,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeKind {
    CppQuote,
    CppSystem,
    AsmQuote,
}

impl WorkspaceIndex {
    /// Resolve `#include` / `.include` target on `line` (0-based) to `(kind, display_target, resolved_path)`.
    pub fn resolve_include_on_line(
        &mut self,
        uri: &Uri,
        line: u32,
        ctx: &mut CompileContext,
    ) -> Option<(IncludeKind, String, PathBuf)> {
        let file_ctx = ctx.for_uri(uri);
        self.sync_preprocessed(uri, file_ctx.preprocessed);
        let file = self.file_by_uri(uri)?.clone();
        let (kind, inc) = parse_include_directive_in_file(&file, line, file_ctx.preprocessed)?;
        let cur_dir = file.path.parent();
        let resolved = resolve_include_path(
            &inc,
            cur_dir,
            &file_ctx.quote_dirs,
            &file_ctx.include_dirs,
            self.workspace_root.as_deref(),
        )?;
        Some((kind, inc.target, self.display_path(&resolved)))
    }

    /// Parse an `#include` / `.include` directive on `line` (0-based) even if the target
    /// file could not be resolved on disk.
    pub fn parse_include_on_line(
        &self,
        uri: &Uri,
        line: u32,
        preprocessed: bool,
    ) -> Option<(IncludeKind, String)> {
        let file = self.file_by_uri(uri)?;
        let (kind, inc) = parse_include_directive_in_file(file, line, preprocessed)?;
        Some((kind, inc.target))
    }

    /// Collect known constant `#define` and `.equ`/`.set`/`=` definitions visible from `uri`,
    /// stopping assignments in the current file at `at_offset` when provided.
    pub fn constant_defines_in_scope(
        &mut self,
        uri: &Uri,
        at_offset: Option<usize>,
        ctx: &mut CompileContext,
    ) -> HashMap<String, String> {
        let pre = ctx.for_uri(uri).preprocessed;
        self.sync_preprocessed(uri, pre);
        let mut consts = HashMap::new();
        let Some(file) = self.file_by_uri(uri).cloned() else {
            return consts;
        };
        let closure = self.resolve_include_closure(&file.path, ctx);
        for path in closure.into_iter().rev() {
            if let Some(f) = self.files.get(&path) {
                let mut defs: Vec<&SymbolDef> = f
                    .definitions
                    .iter()
                    .filter(|d| matches!(d.kind, SymKind::Equ | SymKind::Define))
                    .collect();
                defs.sort_by_key(|d| d.name_span.0);
                let is_cur = path == file.path;

                for def in defs {
                    if is_cur
                        && let Some(off) = at_offset
                        && def.full_span.1 > off
                    {
                        continue;
                    }
                    let Some(val) = &def.value_text else {
                        continue;
                    };
                    // Skip function-like `#define FOO(x) ...`
                    if def.kind == SymKind::Define
                        && def
                            .line_text
                            .find(&def.name)
                            .and_then(|pos| def.line_text.as_bytes().get(pos + def.name.len()))
                            == Some(&b'(')
                    {
                        continue;
                    }
                    if contains_ident_word(val, &def.name) {
                        consts.remove(&def.name);
                        continue;
                    }
                    consts.insert(def.name.clone(), val.clone());
                }
            }
        }
        consts
    }

    /// Ensure included files referenced by `file_path` (including headers, lazily)
    /// are indexed if readable from disk.
    pub fn resolve_include_closure(
        &mut self,
        start_path: &Path,
        ctx: &mut CompileContext,
    ) -> Vec<PathBuf> {
        let start_key = path_key(start_path);
        let mut visited = HashSet::new();
        let mut ordered = Vec::new();
        let mut queue = VecDeque::new();
        visited.insert(start_key.clone());
        queue.push_back(start_key);

        while let Some(cur_key) = queue.pop_front() {
            let pre = self
                .files
                .get(&cur_key)
                .map(|f| ctx.for_uri(&f.uri).preprocessed)
                .unwrap_or_else(|| ctx.is_preprocessed_path(&cur_key));
            self.sync_preprocessed_path(&cur_key, pre);
            ordered.push(cur_key.clone());
            let (includes, cur_uri, cur_dir) = match self.files.get(&cur_key) {
                Some(f) => {
                    let disk_dir = crate::context::uri_to_path(&f.uri)
                        .and_then(|p| p.parent().map(Path::to_path_buf))
                        .or_else(|| cur_key.parent().map(Path::to_path_buf));
                    (f.includes.clone(), Some(f.uri.clone()), disk_dir)
                }
                None => (
                    Vec::new(),
                    path_to_uri(&cur_key),
                    cur_key.parent().map(Path::to_path_buf),
                ),
            };
            if includes.is_empty() {
                continue;
            }
            let file_ctx = cur_uri.as_ref().map(|u| ctx.for_uri(u));

            for inc in includes {
                if let Some(resolved) = resolve_include_path(
                    &inc,
                    cur_dir.as_deref(),
                    file_ctx
                        .as_ref()
                        .map(|c| c.quote_dirs.as_slice())
                        .unwrap_or(&[]),
                    file_ctx
                        .as_ref()
                        .map(|c| c.include_dirs.as_slice())
                        .unwrap_or(&[]),
                    self.workspace_root.as_deref(),
                ) {
                    let res_key = path_key(&resolved);
                    let res_pre = ctx.is_preprocessed_path(&resolved);
                    if !self.files.contains_key(&res_key) {
                        self.index_path_from_disk_with_preprocessed(&resolved, Some(res_pre));
                    }
                    if visited.insert(res_key.clone()) {
                        queue.push_back(res_key);
                    }
                }
            }
        }
        ordered
    }

    /// Find files in the workspace that directly or transitively include `target_path`.
    pub fn files_including(
        &mut self,
        target_path: &Path,
        ctx: &mut CompileContext,
    ) -> Vec<PathBuf> {
        let target_key = path_key(target_path);
        let candidate_paths: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(p, f)| **p == target_key || !f.includes.is_empty())
            .map(|(p, _)| p.clone())
            .collect();
        let mut matching = Vec::new();
        for p in candidate_paths {
            let closure = self.resolve_include_closure(&p, ctx);
            if closure.contains(&target_key) {
                matching.push(p);
            }
        }
        matching
    }

    pub fn is_inside_workspace(&self, path: &Path) -> bool {
        let Some(norm_root) = &self.canon_workspace_root else {
            return true;
        };
        let key = path_key(path);
        crate::context::path_starts_with(&key, norm_root)
    }

    pub fn token_hover_info(
        &mut self,
        uri: &Uri,
        offset: usize,
        ctx: &mut CompileContext,
    ) -> Option<SymbolHoverInfo> {
        let pre = ctx.for_uri(uri).preprocessed;
        self.sync_preprocessed(uri, pre);
        let file = self.file_by_uri(uri)?.clone();
        let (tok_start, tok_end) = word_range_at(&file.text, offset);
        if tok_start == tok_end {
            return None;
        }
        let token = file.text.get(tok_start..tok_end)?;
        let range = file.range((tok_start, tok_end));

        if let Some(info) = hover_numeric_or_param(&file, token, offset, tok_start, range) {
            return Some(info);
        }
        if token.starts_with(".L")
            && let Some(def) = file
                .definitions
                .iter()
                .find(|d| d.kind == SymKind::LocalLabel && d.name == token)
        {
            return Some(hover_from_def(&file, def, range, 1));
        }
        self.hover_scoped_or_global(&file, token, (tok_start, tok_end), range, ctx)
    }

    fn hover_scoped_or_global(
        &mut self,
        file: &FileIndex,
        token: &str,
        tok_span: (usize, usize),
        range: Range,
        ctx: &mut CompileContext,
    ) -> Option<SymbolHoverInfo> {
        let closure = self.resolve_include_closure(&file.path, ctx);
        let mut first_non_equ: Option<(&FileIndex, &SymbolDef)> = None;
        let mut equ_candidates: Vec<(&FileIndex, &SymbolDef)> = Vec::new();
        for inc_path in &closure {
            if let Some(inc_file) = self.files.get(inc_path) {
                for d in &inc_file.definitions {
                    if d.name != token {
                        continue;
                    }
                    if d.kind == SymKind::Equ {
                        equ_candidates.push((inc_file, d));
                    } else if matches!(d.kind, SymKind::Macro | SymKind::Define)
                        && first_non_equ.is_none()
                    {
                        first_non_equ = Some((inc_file, d));
                    }
                }
            }
        }
        if !equ_candidates.is_empty() {
            let count = equ_candidates.len();
            let chosen = select_effective_equ(&file.path, tok_span.0, tok_span.1, &equ_candidates);
            return Some(hover_from_def(chosen.0, chosen.1, range, count));
        }
        if let Some((inc_file, def)) = first_non_equ {
            return Some(hover_from_def(inc_file, def, range, 1));
        }
        self.files
            .values()
            .filter_map(|f| {
                f.definitions
                    .iter()
                    .find(|d| d.kind == SymKind::Label && d.name == token)
                    .map(|d| (f, d))
            })
            .next()
            .map(|(f, def)| hover_from_def(f, def, range, 1))
    }

    pub fn visible_symbols_for_completion(
        &mut self,
        uri: &Uri,
        offset: usize,
        ctx: &mut CompileContext,
    ) -> VisibleCompletions {
        let pre = ctx.for_uri(uri).preprocessed;
        self.sync_preprocessed(uri, pre);
        let mut symbols = Vec::new();
        let mut macros = Vec::new();
        let mut seen_syms = HashSet::new();
        let mut seen_macros = HashSet::new();

        let Some(cur_file) = self.file_by_uri(uri).cloned() else {
            return VisibleCompletions { symbols, macros };
        };

        // Current file local labels & enclosing macro / .irp / .irpc params
        for def in &cur_file.definitions {
            if def.kind == SymKind::LocalLabel && seen_syms.insert(def.name.clone()) {
                symbols.push(CompletionSymbol {
                    name: def.name.clone(),
                    kind: def.kind,
                    detail: def.value_text.clone(),
                });
            }
        }
        for p in visible_params_at(&cur_file, offset) {
            let param_ref = format!("\\{}", p.name);
            if seen_syms.insert(param_ref.clone()) {
                symbols.push(CompletionSymbol {
                    name: param_ref,
                    kind: SymKind::MacroParam,
                    detail: None,
                });
            }
        }

        // Include closure (.equ, .macro, #define)
        let closure = self.resolve_include_closure(&cur_file.path, ctx);
        for path in closure {
            if let Some(f) = self.files.get(&path) {
                for def in &f.definitions {
                    match def.kind {
                        SymKind::Macro if seen_macros.insert(def.name.clone()) => {
                            macros.push(def.name.clone());
                        }
                        SymKind::Equ | SymKind::Define if seen_syms.insert(def.name.clone()) => {
                            symbols.push(CompletionSymbol {
                                name: def.name.clone(),
                                kind: def.kind,
                                detail: def.value_text.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // Project-wide ordinary labels
        for f in self.files.values() {
            for def in &f.definitions {
                if def.kind == SymKind::Label && seen_syms.insert(def.name.clone()) {
                    symbols.push(CompletionSymbol {
                        name: def.name.clone(),
                        kind: def.kind,
                        detail: None,
                    });
                }
            }
        }

        VisibleCompletions { symbols, macros }
    }

    /// Map visible symbol names to their definition kinds for semantic highlighting.
    pub fn symbol_kinds_in_scope(
        &mut self,
        uri: &Uri,
        ctx: &mut CompileContext,
    ) -> HashMap<String, SymKind> {
        let pre = ctx.for_uri(uri).preprocessed;
        self.sync_preprocessed(uri, pre);
        let mut out = HashMap::new();
        let Some(cur_file) = self.file_by_uri(uri).cloned() else {
            return out;
        };

        let closure = self.resolve_include_closure(&cur_file.path, ctx);
        for path in closure {
            if let Some(f) = self.files.get(&path) {
                for def in &f.definitions {
                    if matches!(def.kind, SymKind::Equ | SymKind::Define | SymKind::Macro) {
                        out.entry(def.name.clone()).or_insert(def.kind);
                    }
                }
            }
        }

        for def in &cur_file.definitions {
            if matches!(def.kind, SymKind::Label | SymKind::LocalLabel) {
                out.entry(def.name.clone()).or_insert(def.kind);
            }
        }

        for f in self.files.values() {
            for def in &f.definitions {
                if def.kind == SymKind::Label {
                    out.entry(def.name.clone()).or_insert(SymKind::Label);
                }
            }
            for g in &f.globals {
                out.entry(g.clone()).or_insert(SymKind::Label);
            }
        }

        out
    }
}

fn parse_include_directive_in_file(
    file: &FileIndex,
    line: u32,
    preprocessed: bool,
) -> Option<(IncludeKind, super::IncludeDirective)> {
    let (ls, le, _) = file.line_index.line_bounds(&file.text, line as usize);
    let mut in_block = crate::line::in_block_comment_before(&file.text, ls, preprocessed);
    let raw_line = file.text.get(ls..le)?;
    let stripped = crate::line::strip_comments_with_state(raw_line, &mut in_block, preprocessed);
    let trimmed = stripped.trim();
    if preprocessed && let Some(cpp_inc) = parse_cpp_include_line(trimmed) {
        let kind = if cpp_inc.is_system {
            IncludeKind::CppSystem
        } else {
            IncludeKind::CppQuote
        };
        return Some((kind, cpp_inc));
    }
    Some((IncludeKind::AsmQuote, parse_asm_include_line(trimmed)?))
}

fn hover_numeric_or_param(
    file: &FileIndex,
    token: &str,
    offset: usize,
    tok_start: usize,
    range: Range,
) -> Option<SymbolHoverInfo> {
    if let Some((digits, forward)) = parse_numeric_ref_token(token)
        && let Some(target_def) = resolve_numeric_def(file, digits, forward, tok_start)
    {
        let target_pos = file
            .line_index
            .offset_to_position(&file.text, target_def.name_span.0);
        return Some(SymbolHoverInfo {
            name: token.to_owned(),
            kind: SymKind::NumericLabel,
            assign_kind: None,
            assignment_count: 1,
            param_label: None,
            def_line: target_def.line_text.clone(),
            value: None,
            target_uri: Some(file.uri.clone()),
            target_line_1based: Some(target_pos.line + 1),
            range,
        });
    }
    if let Some(scope) = file
        .repeat_scopes
        .iter()
        .rev()
        .find(|s| offset >= s.param.span.0 && offset <= s.param.span.1)
    {
        let label = if scope.directive == ".irpc" {
            ".irpc parameter"
        } else {
            ".irp parameter"
        };
        let line = file.line_index.line_at_offset(scope.param.span.0) + 1;
        return Some(SymbolHoverInfo {
            name: scope.param.name.clone(),
            kind: SymKind::MacroParam,
            assign_kind: None,
            assignment_count: 1,
            param_label: Some(label.to_owned()),
            def_line: scope.line_text.clone(),
            value: None,
            target_uri: Some(file.uri.clone()),
            target_line_1based: Some(line as u32),
            range,
        });
    }
    if let Some(param_name) = parse_macro_var_token(token)
        && let Some((_, param, def_line, kind_label)) =
            resolve_param_scope(file, tok_start, param_name)
    {
        let line = file.line_index.line_at_offset(param.span.0) + 1;
        return Some(SymbolHoverInfo {
            name: param.name.clone(),
            kind: SymKind::MacroParam,
            assign_kind: None,
            assignment_count: 1,
            param_label: Some(kind_label.to_owned()),
            def_line: def_line.to_owned(),
            value: None,
            target_uri: Some(file.uri.clone()),
            target_line_1based: Some(line as u32),
            range,
        });
    }
    None
}

fn select_effective_equ<'a>(
    cur_path: &Path,
    tok_start: usize,
    tok_end: usize,
    candidates: &[(&'a FileIndex, &'a SymbolDef)],
) -> (&'a FileIndex, &'a SymbolDef) {
    // 1. If cursor is directly on the LHS of an assignment in the current file, show that one.
    if let Some(exact) = candidates
        .iter()
        .find(|(f, d)| f.path == cur_path && tok_start >= d.name_span.0 && tok_end <= d.name_span.1)
    {
        return *exact;
    }

    // 2. Otherwise pick the latest assignment in the current file completed before `tok_start`.
    if let Some(prior) = candidates
        .iter()
        .filter(|(f, d)| f.path == cur_path && d.full_span.1 <= tok_start)
        .max_by_key(|(_, d)| d.name_span.0)
    {
        return *prior;
    }

    // 3. Fall back to included files (last assignment in closure) or the earliest local definition.
    if let Some(from_inc) = candidates.iter().rfind(|(f, _)| f.path != cur_path) {
        return *from_inc;
    }

    candidates[0]
}

fn contains_ident_word(haystack: &str, ident: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut i = 0;
    while let Some(pos) = haystack[i..].find(ident) {
        let start = i + pos;
        let end = start + ident.len();
        let before_ok =
            start == 0 || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_');
        let after_ok =
            end >= bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
    }
    false
}

fn hover_from_def(
    file: &FileIndex,
    def: &SymbolDef,
    range: Range,
    assignment_count: usize,
) -> SymbolHoverInfo {
    let line_1based = file.line_index.line_at_offset(def.name_span.0) as u32 + 1;
    SymbolHoverInfo {
        name: def.name.clone(),
        kind: def.kind,
        assign_kind: def.assign_kind,
        assignment_count,
        param_label: None,
        def_line: def.line_text.clone(),
        value: def.value_text.clone(),
        target_uri: Some(file.uri.clone()),
        target_line_1based: Some(line_1based),
        range,
    }
}

pub fn visible_params_at(file: &FileIndex, offset: usize) -> Vec<&MacroParamDef> {
    let mut out = Vec::new();
    for s in &file.repeat_scopes {
        if offset >= s.full_span.0 && offset <= s.full_span.1 {
            out.push(&s.param);
        }
    }
    for d in &file.definitions {
        if d.kind == SymKind::Macro && offset >= d.full_span.0 && offset <= d.full_span.1 {
            for p in &d.params {
                out.push(p);
            }
        }
    }
    out
}

pub fn resolve_param_scope<'a>(
    file: &'a FileIndex,
    offset: usize,
    param_name: &str,
) -> Option<(ParamScope, &'a MacroParamDef, &'a str, &'static str)> {
    if let Some((idx, scope)) = file.repeat_scopes.iter().enumerate().rev().find(|(_, s)| {
        offset >= s.full_span.0 && offset <= s.full_span.1 && s.param.name == param_name
    }) {
        let label = if scope.directive == ".irpc" {
            ".irpc parameter"
        } else {
            ".irp parameter"
        };
        return Some((
            ParamScope::Repeat(idx),
            &scope.param,
            scope.line_text.as_str(),
            label,
        ));
    }

    if let Some((idx, macro_def, p)) =
        file.definitions
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, d)| {
                if d.kind == SymKind::Macro && offset >= d.full_span.0 && offset <= d.full_span.1 {
                    d.params
                        .iter()
                        .find(|p| p.name == param_name)
                        .map(|p| (i, d, p))
                } else {
                    None
                }
            })
    {
        return Some((
            ParamScope::Macro(idx),
            p,
            macro_def.line_text.as_str(),
            "macro parameter",
        ));
    }

    None
}

pub fn enclosing_macro_at(file: &FileIndex, offset: usize) -> Option<usize> {
    file.definitions.iter().position(|d| {
        d.kind == SymKind::Macro && offset >= d.full_span.0 && offset <= d.full_span.1
    })
}

pub fn resolve_numeric_def<'a>(
    file: &'a FileIndex,
    digits: &str,
    forward: bool,
    ref_offset: usize,
) -> Option<&'a SymbolDef> {
    if forward {
        file.definitions
            .iter()
            .filter(|d| {
                d.kind == SymKind::NumericLabel && d.name == digits && d.name_span.0 > ref_offset
            })
            .min_by_key(|d| d.name_span.0)
    } else {
        file.definitions
            .iter()
            .filter(|d| {
                d.kind == SymKind::NumericLabel && d.name == digits && d.name_span.0 < ref_offset
            })
            .max_by_key(|d| d.name_span.0)
    }
}

pub fn parse_numeric_ref_token(token: &str) -> Option<(&str, bool)> {
    let bytes = token.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let last = *bytes.last()?;
    let forward = match last {
        b'f' | b'F' => true,
        b'b' | b'B' => false,
        _ => return None,
    };
    let digits = &token[..token.len() - 1];
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        Some((digits, forward))
    } else {
        None
    }
}

pub fn parse_macro_var_token(token: &str) -> Option<&str> {
    let rest = token.strip_prefix('\\')?;
    let trimmed = rest.split("\\()").next().unwrap_or(rest);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
