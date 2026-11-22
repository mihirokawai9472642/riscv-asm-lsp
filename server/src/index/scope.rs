use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use lsp_types::{Range, Uri};

use crate::context::{CompileContext, normalize_path, path_to_uri};
use crate::position::word_range_at;

use super::parse::{parse_asm_include_line, parse_cpp_include_line, resolve_include_path};
use super::{
    CompletionSymbol, FileIndex, SymKind, SymbolDef, SymbolHoverInfo, VisibleCompletions,
    WorkspaceIndex,
};

impl WorkspaceIndex {
    /// Resolve `#include` / `.include` target on `line` (0-based) to `(display_target, resolved_path)`.
    pub fn resolve_include_on_line(
        &mut self,
        uri: &Uri,
        line: u32,
        ctx: &mut CompileContext,
    ) -> Option<(String, PathBuf)> {
        let file = self.file_by_uri(uri)?.clone();
        let (ls, le, _) = file.line_index.line_bounds(&file.text, line as usize);
        let trimmed = file.text.get(ls..le)?.trim();
        let inc = parse_cpp_include_line(trimmed).or_else(|| parse_asm_include_line(trimmed))?;
        let file_ctx = ctx.for_uri(uri);
        let cur_dir = file.path.parent();
        let resolved = resolve_include_path(
            &inc,
            cur_dir,
            &file_ctx.quote_dirs,
            &file_ctx.include_dirs,
            self.workspace_root.as_deref(),
        )?;
        Some((inc.target, normalize_path(&resolved)))
    }

    /// Collect known constant `#define` and `.equ`/`.set`/`=` definitions visible from `uri`.
    pub fn constant_defines_in_scope(
        &mut self,
        uri: &Uri,
        ctx: &mut CompileContext,
    ) -> HashMap<String, String> {
        let mut consts = HashMap::new();
        let Some(file) = self.file_by_uri(uri).cloned() else {
            return consts;
        };
        let closure = self.resolve_include_closure(&file.path, ctx);
        for path in closure {
            if let Some(f) = self.files.get(&path) {
                for def in &f.definitions {
                    if matches!(def.kind, SymKind::Equ | SymKind::Define)
                        && let Some(val) = &def.value_text
                    {
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
                        consts
                            .entry(def.name.clone())
                            .or_insert_with(|| val.clone());
                    }
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
        let start_norm = normalize_path(start_path);
        let mut visited = HashSet::new();
        let mut ordered = Vec::new();
        let mut queue = VecDeque::new();
        visited.insert(start_norm.clone());
        queue.push_back(start_norm);

        while let Some(cur) = queue.pop_front() {
            ordered.push(cur.clone());
            let includes = self
                .files
                .get(&cur)
                .map(|f| f.includes.clone())
                .unwrap_or_default();
            if includes.is_empty() {
                continue;
            }
            let file_ctx = path_to_uri(&cur).map(|u| ctx.for_uri(&u));
            let cur_dir = cur.parent().map(Path::to_path_buf);

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
                    let res_norm = normalize_path(&resolved);
                    if !self.files.contains_key(&res_norm) {
                        self.index_path_from_disk(&res_norm);
                    }
                    if visited.insert(res_norm.clone()) {
                        queue.push_back(res_norm);
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
        let target_norm = normalize_path(target_path);
        let all_paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        let mut matching = Vec::new();
        for p in all_paths {
            let closure = self.resolve_include_closure(&p, ctx);
            if closure.contains(&target_norm) {
                matching.push(p);
            }
        }
        matching
    }

    pub fn is_inside_workspace(&self, path: &Path) -> bool {
        let Some(root) = &self.workspace_root else {
            return true;
        };
        let norm_root = normalize_path(root);
        let norm_path = normalize_path(path);
        norm_path.starts_with(&norm_root)
    }

    pub fn token_hover_info(
        &mut self,
        uri: &Uri,
        offset: usize,
        ctx: &mut CompileContext,
    ) -> Option<SymbolHoverInfo> {
        let file = self.file_by_uri(uri)?.clone();
        let (tok_start, tok_end) = word_range_at(&file.text, offset);
        if tok_start == tok_end {
            return None;
        }
        let token = file.text.get(tok_start..tok_end)?;
        let range = file.range((tok_start, tok_end));

        // Numeric label reference (`1f` / `1b`)
        if let Some((digits, forward)) = parse_numeric_ref_token(token)
            && let Some(target_def) = resolve_numeric_def(&file, digits, forward, tok_start)
        {
            let target_pos = file
                .line_index
                .offset_to_position(&file.text, target_def.name_span.0);
            return Some(SymbolHoverInfo {
                name: token.to_owned(),
                kind: SymKind::NumericLabel,
                def_line: target_def.line_text.clone(),
                value: None,
                target_uri: Some(file.uri.clone()),
                target_line_1based: Some(target_pos.line + 1),
                range,
            });
        }

        // Macro parameter (`\reg`)
        if let Some(param_name) = parse_macro_var_token(token)
            && let Some(macro_idx) = enclosing_macro_at(&file, tok_start)
            && let Some(macro_def) = file.definitions.get(macro_idx)
            && let Some(p) = macro_def.params.iter().find(|p| p.name == param_name)
        {
            return Some(SymbolHoverInfo {
                name: p.name.clone(),
                kind: SymKind::MacroParam,
                def_line: macro_def.line_text.clone(),
                value: None,
                target_uri: Some(file.uri.clone()),
                target_line_1based: Some(
                    file.line_index
                        .offset_to_position(&file.text, p.span.0)
                        .line
                        + 1,
                ),
                range,
            });
        }

        // Local label (`.L...`)
        if token.starts_with(".L")
            && let Some(def) = file
                .definitions
                .iter()
                .find(|d| d.kind == SymKind::LocalLabel && d.name == token)
        {
            return Some(hover_from_def(&file, def, range));
        }

        // Included `.equ` / `.macro` / `#define` (starts with `file.path` itself)
        let closure = self.resolve_include_closure(&file.path, ctx);
        for inc_path in &closure {
            if let Some(inc_file) = self.files.get(inc_path)
                && let Some(def) = inc_file.definitions.iter().find(|d| {
                    matches!(d.kind, SymKind::Equ | SymKind::Macro | SymKind::Define)
                        && d.name == token
                })
            {
                return Some(hover_from_def(inc_file, def, range));
            }
        }

        // Project-wide ordinary label
        if let Some(def_file_and_def) = self
            .files
            .values()
            .filter_map(|f| {
                f.definitions
                    .iter()
                    .find(|d| d.kind == SymKind::Label && d.name == token)
                    .map(|d| (f, d))
            })
            .next()
        {
            return Some(hover_from_def(
                def_file_and_def.0,
                def_file_and_def.1,
                range,
            ));
        }

        None
    }

    pub fn visible_symbols_for_completion(
        &mut self,
        uri: &Uri,
        offset: usize,
        ctx: &mut CompileContext,
    ) -> VisibleCompletions {
        let mut symbols = Vec::new();
        let mut macros = Vec::new();
        let mut seen_syms = HashSet::new();
        let mut seen_macros = HashSet::new();

        let Some(cur_file) = self.file_by_uri(uri).cloned() else {
            return VisibleCompletions { symbols, macros };
        };

        // Current file local labels & macro params
        for def in &cur_file.definitions {
            if def.kind == SymKind::LocalLabel && seen_syms.insert(def.name.clone()) {
                symbols.push(CompletionSymbol {
                    name: def.name.clone(),
                    kind: def.kind,
                    detail: def.value_text.clone(),
                });
            }
        }
        if let Some(macro_idx) = enclosing_macro_at(&cur_file, offset)
            && let Some(macro_def) = cur_file.definitions.get(macro_idx)
        {
            for p in &macro_def.params {
                let param_ref = format!("\\{}", p.name);
                if seen_syms.insert(param_ref.clone()) {
                    symbols.push(CompletionSymbol {
                        name: param_ref,
                        kind: SymKind::MacroParam,
                        detail: None,
                    });
                }
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

fn hover_from_def(file: &FileIndex, def: &SymbolDef, range: Range) -> SymbolHoverInfo {
    let line_1based = file
        .line_index
        .offset_to_position(&file.text, def.name_span.0)
        .line
        + 1;
    SymbolHoverInfo {
        name: def.name.clone(),
        kind: def.kind,
        def_line: def.line_text.clone(),
        value: def.value_text.clone(),
        target_uri: Some(file.uri.clone()),
        target_line_1based: Some(line_1based),
        range,
    }
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
