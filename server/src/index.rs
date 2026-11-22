use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::{Location, Range, Uri};
use tree_sitter::{Node, Parser};

use crate::context::{CompileContext, normalize_path, path_to_uri, uri_to_path};
use crate::document::Document;
use crate::position::{LineIndex, clamp_to_char_boundary, word_range_at};

const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    Label,
    LocalLabel,
    NumericLabel,
    Equ,
    Macro,
    MacroParam,
    Define,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Identifier,
    NumericForward,
    NumericBackward,
    MacroParam,
}

#[derive(Debug, Clone)]
pub struct MacroParamDef {
    pub name: String,
    pub span: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymKind,
    pub name_span: (usize, usize),
    pub full_span: (usize, usize),
    pub line_text: String,
    pub value_text: Option<String>,
    pub params: Vec<MacroParamDef>,
    pub enclosing_label_idx: Option<usize>,
    pub enclosing_macro_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub name: String,
    pub kind: RefKind,
    pub span: (usize, usize),
    pub enclosing_macro_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct IncludeDirective {
    pub target: String,
    pub is_system: bool,
}

#[derive(Debug, Clone)]
pub struct FileIndex {
    pub path: PathBuf,
    pub uri: Uri,
    pub is_header: bool,
    pub text: String,
    pub line_index: LineIndex,
    pub definitions: Vec<SymbolDef>,
    pub references: Vec<SymbolRef>,
    pub includes: Vec<IncludeDirective>,
    pub globals: HashSet<String>,
}

impl FileIndex {
    pub fn location(&self, span: (usize, usize)) -> Location {
        Location::new(
            self.uri.clone(),
            self.line_index
                .byte_span_to_range(&self.text, span.0, span.1),
        )
    }

    pub fn range(&self, span: (usize, usize)) -> Range {
        self.line_index
            .byte_span_to_range(&self.text, span.0, span.1)
    }
}

#[derive(Debug, Clone)]
pub struct CompletionSymbol {
    pub name: String,
    pub kind: SymKind,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VisibleCompletions {
    pub symbols: Vec<CompletionSymbol>,
    pub macros: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SymbolHoverInfo {
    pub name: String,
    pub kind: SymKind,
    pub def_line: String,
    pub value: Option<String>,
    pub target_uri: Option<Uri>,
    pub target_line_1based: Option<u32>,
    pub range: Range,
}

pub struct WorkspaceIndex {
    workspace_root: Option<PathBuf>,
    files: HashMap<PathBuf, FileIndex>,
    parser: Parser,
}

impl WorkspaceIndex {
    pub fn new(workspace_root: Option<PathBuf>) -> Result<Self, String> {
        let mut parser = Parser::new();
        let language = tree_sitter_riscv::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(|err| format!("failed to initialize tree-sitter-riscv: {err}"))?;
        let mut idx = Self {
            workspace_root,
            files: HashMap::new(),
            parser,
        };
        idx.scan_workspace();
        Ok(idx)
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub fn files(&self) -> impl Iterator<Item = &FileIndex> {
        self.files.values()
    }

    pub fn file_by_uri(&self, uri: &Uri) -> Option<&FileIndex> {
        let path = uri_to_path(uri)?;
        let norm = normalize_path(&path);
        self.files.get(&norm).or_else(|| self.files.get(&path))
    }

    pub fn scan_workspace(&mut self) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        let mut candidates = Vec::new();
        collect_source_files(&root, &mut candidates);
        for path in candidates {
            self.index_path_from_disk(&path);
        }
    }

    pub fn upsert_open_document(&mut self, doc: &Document) {
        let Some(path) = uri_to_path(&doc.uri) else {
            return;
        };
        let norm = normalize_path(&path);
        let is_header = is_header_path(&norm);
        let entry = build_file_index(
            norm.clone(),
            doc.uri.clone(),
            is_header,
            doc.text.clone(),
            doc.line_index.clone(),
            doc.tree.as_ref(),
            &mut self.parser,
        );
        self.files.insert(norm, entry);
    }

    pub fn reload_path(&mut self, uri: &Uri) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let norm = normalize_path(&path);
        if norm.exists() {
            self.index_path_from_disk(&norm);
        } else {
            self.files.remove(&norm);
        }
    }

    pub fn remove_path(&mut self, uri: &Uri) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let norm = normalize_path(&path);
        self.files.remove(&norm);
        self.files.remove(&path);
    }

    /// Resolve a `#include` or `.include` target on `line` (0-based) to `(display_target, resolved_path)`.
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

    fn index_path_from_disk(&mut self, path: &Path) {
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        if !meta.is_file() || meta.len() > MAX_INDEX_FILE_BYTES {
            return;
        }
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let norm = normalize_path(path);
        let Some(uri) = path_to_uri(&norm) else {
            return;
        };
        let line_index = LineIndex::new(&text);
        let is_header = is_header_path(&norm);
        let entry = build_file_index(
            norm.clone(),
            uri,
            is_header,
            text,
            line_index,
            None,
            &mut self.parser,
        );
        self.files.insert(norm, entry);
    }

    /// Ensure included files referenced by `file_path` (even outside workspace, e.g. `-I` dirs)
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

        // Included `.equ` / `.macro` / `#define`
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

fn is_header_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("h" | "H" | "hpp")
    )
}

fn is_asm_or_header_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("S" | "s" | "inc" | "h" | "H")
    )
}

fn collect_source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if matches!(
                name,
                ".git" | ".cache" | "target" | "build" | "node_modules"
            ) {
                continue;
            }
            collect_source_files(&path, out);
        } else if is_asm_or_header_path(&path) {
            out.push(path);
        }
    }
}

fn resolve_include_path(
    inc: &IncludeDirective,
    cur_dir: Option<&Path>,
    quote_dirs: &[PathBuf],
    include_dirs: &[PathBuf],
    workspace_root: Option<&Path>,
) -> Option<PathBuf> {
    let rel = Path::new(&inc.target);
    if rel.is_absolute() {
        return rel.exists().then(|| rel.to_path_buf());
    }
    if !inc.is_system
        && let Some(dir) = cur_dir
    {
        let cand = dir.join(rel);
        if cand.exists() {
            return Some(cand);
        }
    }
    if !inc.is_system {
        for qd in quote_dirs {
            let cand = qd.join(rel);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    for idir in include_dirs {
        let cand = idir.join(rel);
        if cand.exists() {
            return Some(cand);
        }
    }
    if let Some(root) = workspace_root {
        let cand = root.join(rel);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

fn build_file_index(
    path: PathBuf,
    uri: Uri,
    is_header: bool,
    text: String,
    line_index: LineIndex,
    existing_tree: Option<&tree_sitter::Tree>,
    parser: &mut Parser,
) -> FileIndex {
    if is_header {
        return parse_header_file(path, uri, text, line_index);
    }

    let owned_tree = if existing_tree.is_none() {
        parser.parse(&text, None)
    } else {
        None
    };
    let tree = existing_tree.or(owned_tree.as_ref());

    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut includes = Vec::new();
    let mut globals = HashSet::new();

    // Scan preprocessor `#include` lines (tree-sitter treats `#...` as comment).
    scan_cpp_includes(&text, &mut includes);

    if let Some(tree) = tree {
        let mut ctx = ExtractCtx {
            text: &text,
            line_index: &line_index,
            definitions: &mut definitions,
            references: &mut references,
            includes: &mut includes,
            globals: &mut globals,
            macro_stack: Vec::new(),
        };
        walk_ast(tree.root_node(), &mut ctx);
        // Close any unclosed macros cleanly at EOF
        for open_idx in ctx.macro_stack {
            if let Some(def) = ctx.definitions.get_mut(open_idx) {
                def.full_span.1 = text.len();
            }
        }
    }

    // Fallback scan for top-level `SYM = expr` assignments if tree-sitter flagged ERROR
    scan_fallback_assignments(&text, &line_index, &mut definitions);

    finalize_label_hierarchy_and_ranges(&text, &mut definitions, &mut references);

    FileIndex {
        path,
        uri,
        is_header: false,
        text,
        line_index,
        definitions,
        references,
        includes,
        globals,
    }
}

fn parse_header_file(path: PathBuf, uri: Uri, text: String, line_index: LineIndex) -> FileIndex {
    let mut definitions = Vec::new();
    let mut includes = Vec::new();
    let mut line_start = 0usize;

    for raw_line in text.split_inclusive('\n') {
        let line_end = line_start + raw_line.len();
        let trimmed = raw_line.trim();
        if let Some(inc) = parse_cpp_include_line(trimmed) {
            includes.push(inc);
        } else if let Some(rest) = trimmed.strip_prefix('#')
            && let Some(after_def) = rest.trim_start().strip_prefix("define")
            && after_def.chars().next().is_some_and(char::is_whitespace)
        {
            let after_ws = after_def.trim_start();
            let ident: String = after_ws
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty()
                && let Some(rel) = raw_line.find(&ident)
            {
                let name_start = line_start + rel;
                let name_end = name_start + ident.len();
                let value_part = after_ws[ident.len()..].trim();
                let value_text = if value_part.is_empty() {
                    None
                } else {
                    Some(value_part.to_owned())
                };
                definitions.push(SymbolDef {
                    name: ident,
                    kind: SymKind::Define,
                    name_span: (name_start, name_end),
                    full_span: (line_start, line_end),
                    line_text: trimmed.to_owned(),
                    value_text,
                    params: Vec::new(),
                    enclosing_label_idx: None,
                    enclosing_macro_idx: None,
                });
            }
        }
        line_start = line_end;
    }

    FileIndex {
        path,
        uri,
        is_header: true,
        text,
        line_index,
        definitions,
        references: Vec::new(),
        includes,
        globals: HashSet::new(),
    }
}

fn scan_cpp_includes(text: &str, out: &mut Vec<IncludeDirective>) {
    for line in text.lines() {
        if let Some(inc) = parse_cpp_include_line(line.trim()) {
            out.push(inc);
        }
    }
}

fn parse_asm_include_line(trimmed: &str) -> Option<IncludeDirective> {
    let rest = trimmed.strip_prefix(".include")?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(IncludeDirective {
        target: inner[..end].to_owned(),
        is_system: false,
    })
}

fn parse_cpp_include_line(trimmed: &str) -> Option<IncludeDirective> {
    let rest = trimmed.strip_prefix('#')?.trim_start();
    let after_inc = rest.strip_prefix("include")?.trim_start();
    if let Some(inner) = after_inc.strip_prefix('"') {
        let end = inner.find('"')?;
        return Some(IncludeDirective {
            target: inner[..end].to_owned(),
            is_system: false,
        });
    }
    if let Some(inner) = after_inc.strip_prefix('<') {
        let end = inner.find('>')?;
        return Some(IncludeDirective {
            target: inner[..end].to_owned(),
            is_system: true,
        });
    }
    None
}

struct ExtractCtx<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
    definitions: &'a mut Vec<SymbolDef>,
    references: &'a mut Vec<SymbolRef>,
    includes: &'a mut Vec<IncludeDirective>,
    globals: &'a mut HashSet<String>,
    macro_stack: Vec<usize>,
}

fn walk_ast(node: Node<'_>, ctx: &mut ExtractCtx<'_>) {
    let kind = node.kind();
    let cur_macro = ctx.macro_stack.last().copied();

    match kind {
        "label" => {
            if let Some(raw) = ctx.text.get(node.start_byte()..node.end_byte()) {
                let name = raw.trim_end_matches(':').trim();
                if !name.is_empty() {
                    let name_start = node.start_byte();
                    let name_end = name_start + name.len();
                    let sym_kind = if name.bytes().all(|b| b.is_ascii_digit()) {
                        SymKind::NumericLabel
                    } else if name.starts_with(".L") {
                        SymKind::LocalLabel
                    } else {
                        SymKind::Label
                    };
                    ctx.definitions.push(SymbolDef {
                        name: name.to_owned(),
                        kind: sym_kind,
                        name_span: (name_start, name_end),
                        full_span: (node.start_byte(), node.end_byte()),
                        line_text: line_text_at(ctx.text, ctx.line_index, name_start),
                        value_text: None,
                        params: Vec::new(),
                        enclosing_label_idx: None,
                        enclosing_macro_idx: None,
                    });
                }
            }
            return;
        }
        "numeric_label" => {
            if let Some(raw) = ctx.text.get(node.start_byte()..node.end_byte()) {
                let name = raw.trim_end_matches(':').trim();
                if !name.is_empty() {
                    let name_start = node.start_byte();
                    let name_end = name_start + name.len();
                    ctx.definitions.push(SymbolDef {
                        name: name.to_owned(),
                        kind: SymKind::NumericLabel,
                        name_span: (name_start, name_end),
                        full_span: (node.start_byte(), node.end_byte()),
                        line_text: line_text_at(ctx.text, ctx.line_index, name_start),
                        value_text: None,
                        params: Vec::new(),
                        enclosing_label_idx: None,
                        enclosing_macro_idx: None,
                    });
                }
            }
            return;
        }
        "directive" => {
            handle_directive(node, ctx);
            return;
        }
        "instruction" => {
            if let Some(mnem) = node.child_by_field_name("mnemonic")
                && let Some(name) = ctx.text.get(mnem.start_byte()..mnem.end_byte())
                && !name.is_empty()
            {
                if let Some(param_name) = parse_macro_var_token(name) {
                    ctx.references.push(SymbolRef {
                        name: param_name.to_owned(),
                        kind: RefKind::MacroParam,
                        span: (mnem.start_byte(), mnem.end_byte()),
                        enclosing_macro_idx: cur_macro,
                    });
                } else {
                    ctx.references.push(SymbolRef {
                        name: name.to_owned(),
                        kind: RefKind::Identifier,
                        span: (mnem.start_byte(), mnem.end_byte()),
                        enclosing_macro_idx: cur_macro,
                    });
                }
            }
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_ast(ops, ctx);
            }
            return;
        }
        "call_statement" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Some(name) = ctx.text.get(name_node.start_byte()..name_node.end_byte())
            {
                ctx.references.push(SymbolRef {
                    name: name.to_owned(),
                    kind: RefKind::Identifier,
                    span: (name_node.start_byte(), name_node.end_byte()),
                    enclosing_macro_idx: cur_macro,
                });
            }
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_ast(ops, ctx);
            }
            return;
        }
        "binary_expression" => {
            if let Some(op) = node.child_by_field_name("operator")
                && op.kind() == "assignment_operator"
                && let Some(left) = node.child_by_field_name("left")
                && left.kind() == "symbol"
                && let Some(name) = ctx.text.get(left.start_byte()..left.end_byte())
            {
                let value_text = node
                    .child_by_field_name("right")
                    .and_then(|r| ctx.text.get(r.start_byte()..r.end_byte()))
                    .map(|s| s.trim().to_owned());
                ctx.definitions.push(SymbolDef {
                    name: name.to_owned(),
                    kind: SymKind::Equ,
                    name_span: (left.start_byte(), left.end_byte()),
                    full_span: (node.start_byte(), node.end_byte()),
                    line_text: line_text_at(ctx.text, ctx.line_index, left.start_byte()),
                    value_text,
                    params: Vec::new(),
                    enclosing_label_idx: None,
                    enclosing_macro_idx: None,
                });
                if let Some(right) = node.child_by_field_name("right") {
                    walk_ast(right, ctx);
                }
                return;
            }
        }
        "symbol" => {
            if let Some(name) = ctx.text.get(node.start_byte()..node.end_byte()) {
                if let Some((digits, forward)) = parse_numeric_ref_token(name) {
                    ctx.references.push(SymbolRef {
                        name: digits.to_owned(),
                        kind: if forward {
                            RefKind::NumericForward
                        } else {
                            RefKind::NumericBackward
                        },
                        span: (node.start_byte(), node.end_byte()),
                        enclosing_macro_idx: cur_macro,
                    });
                } else {
                    ctx.references.push(SymbolRef {
                        name: name.to_owned(),
                        kind: RefKind::Identifier,
                        span: (node.start_byte(), node.end_byte()),
                        enclosing_macro_idx: cur_macro,
                    });
                }
            }
            return;
        }
        "numeric_label_reference" => {
            if let Some(text) = ctx.text.get(node.start_byte()..node.end_byte())
                && let Some((digits, forward)) = parse_numeric_ref_token(text)
            {
                ctx.references.push(SymbolRef {
                    name: digits.to_owned(),
                    kind: if forward {
                        RefKind::NumericForward
                    } else {
                        RefKind::NumericBackward
                    },
                    span: (node.start_byte(), node.end_byte()),
                    enclosing_macro_idx: cur_macro,
                });
            }
            return;
        }
        "macro_variable" | "string_macro_variable" => {
            if let Some(raw) = ctx.text.get(node.start_byte()..node.end_byte())
                && let Some(param) = parse_macro_var_token(raw)
            {
                let start = node.start_byte();
                // Span covering `\param` (avoid including trailing `\()...` in rename span)
                let end = (start + 1 + param.len()).min(node.end_byte());
                ctx.references.push(SymbolRef {
                    name: param.to_owned(),
                    kind: RefKind::MacroParam,
                    span: (start, end),
                    enclosing_macro_idx: cur_macro,
                });
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ast(child, ctx);
    }
}

fn handle_directive(node: Node<'_>, ctx: &mut ExtractCtx<'_>) {
    let Some(mnem_node) = node.child_by_field_name("mnemonic") else {
        return;
    };
    let Some(mnem_raw) = ctx.text.get(mnem_node.start_byte()..mnem_node.end_byte()) else {
        return;
    };
    let mnem = mnem_raw.to_ascii_lowercase();

    if mnem == ".macro" {
        if let Some(name_node) = node.child_by_field_name("name")
            && let Some(name) = ctx.text.get(name_node.start_byte()..name_node.end_byte())
        {
            let mut params = Vec::new();
            if let Some(params_node) = node.child_by_field_name("parameters") {
                collect_macro_params(params_node, ctx.text, &mut params);
            }
            let def_idx = ctx.definitions.len();
            ctx.definitions.push(SymbolDef {
                name: name.to_owned(),
                kind: SymKind::Macro,
                name_span: (name_node.start_byte(), name_node.end_byte()),
                full_span: (node.start_byte(), ctx.text.len()),
                line_text: line_text_at(ctx.text, ctx.line_index, node.start_byte()),
                value_text: None,
                params,
                enclosing_label_idx: None,
                enclosing_macro_idx: None,
            });
            ctx.macro_stack.push(def_idx);
        }
        return;
    }

    if mnem == ".endm" {
        if let Some(open_idx) = ctx.macro_stack.pop()
            && let Some(def) = ctx.definitions.get_mut(open_idx)
        {
            def.full_span.1 = node.end_byte();
        }
        return;
    }

    if matches!(mnem.as_str(), ".equ" | ".set" | ".equiv")
        && let Some(ops_node) = node.child_by_field_name("operands")
    {
        let mut cursor = ops_node.walk();
        let children: Vec<Node<'_>> = ops_node.children(&mut cursor).collect();
        if let Some(sym_node) = children.iter().find(|c| c.kind() == "symbol")
            && let Some(name) = ctx.text.get(sym_node.start_byte()..sym_node.end_byte())
        {
            let sym_id = sym_node.id();
            let value_text = ctx
                .text
                .get(sym_node.end_byte()..ops_node.end_byte())
                .and_then(|rest| rest.split_once(',').map(|(_, v)| v.trim().to_owned()))
                .filter(|s| !s.is_empty());
            ctx.definitions.push(SymbolDef {
                name: name.to_owned(),
                kind: SymKind::Equ,
                name_span: (sym_node.start_byte(), sym_node.end_byte()),
                full_span: (node.start_byte(), node.end_byte()),
                line_text: line_text_at(ctx.text, ctx.line_index, node.start_byte()),
                value_text,
                params: Vec::new(),
                enclosing_label_idx: None,
                enclosing_macro_idx: None,
            });
            // Walk remaining operands (skipping the defined symbol itself) for references
            for child in children {
                if child.id() != sym_id {
                    walk_ast(child, ctx);
                }
            }
            return;
        }
    }

    if mnem == ".include" {
        if let Some(ops_node) = node.child_by_field_name("operands")
            && let Some(raw) = ctx.text.get(ops_node.start_byte()..ops_node.end_byte())
        {
            let trimmed = raw.trim();
            if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                ctx.includes.push(IncludeDirective {
                    target: inner.to_owned(),
                    is_system: false,
                });
            }
        }
        return;
    }

    if matches!(mnem.as_str(), ".globl" | ".global")
        && let Some(ops_node) = node.child_by_field_name("operands")
    {
        let mut cursor = ops_node.walk();
        for child in ops_node.children(&mut cursor) {
            if child.kind() == "symbol"
                && let Some(name) = ctx.text.get(child.start_byte()..child.end_byte())
            {
                ctx.globals.insert(name.to_owned());
            }
        }
    }

    if let Some(ops_node) = node.child_by_field_name("operands") {
        walk_ast(ops_node, ctx);
    }
}

fn collect_macro_params(node: Node<'_>, text: &str, out: &mut Vec<MacroParamDef>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "macro_parameter" {
            if let Some(name_node) = child.child_by_field_name("name")
                && let Some(raw) = text.get(name_node.start_byte()..name_node.end_byte())
            {
                let clean = raw.trim_start_matches(['\\', '%', '$']);
                if !clean.is_empty() {
                    out.push(MacroParamDef {
                        name: clean.to_owned(),
                        span: (name_node.start_byte(), name_node.end_byte()),
                    });
                }
            }
        } else {
            collect_macro_params(child, text, out);
        }
    }
}

fn scan_fallback_assignments(text: &str, line_index: &LineIndex, defs: &mut Vec<SymbolDef>) {
    let mut existing_equs: HashSet<usize> = defs
        .iter()
        .filter(|d| d.kind == SymKind::Equ)
        .map(|d| d.name_span.0)
        .collect();

    let mut offset = 0usize;
    for raw_line in text.split_inclusive('\n') {
        let code = raw_line.split('#').next().unwrap_or(raw_line);
        let code = code.split("//").next().unwrap_or(code);
        if let Some((lhs, rhs)) = code.split_once('=') {
            // Ignore `==`, `!=`, `<=`, `>=`
            let lhs_trim = lhs.trim();
            if !lhs_trim.is_empty()
                && !lhs_trim.ends_with(['!', '<', '>'])
                && !rhs.starts_with('=')
                && is_valid_ident(lhs_trim)
                && let Some(rel) = raw_line.find(lhs_trim)
            {
                let name_start = offset + rel;
                if existing_equs.insert(name_start) {
                    defs.push(SymbolDef {
                        name: lhs_trim.to_owned(),
                        kind: SymKind::Equ,
                        name_span: (name_start, name_start + lhs_trim.len()),
                        full_span: (offset, offset + raw_line.len()),
                        line_text: line_text_at(text, line_index, name_start),
                        value_text: Some(rhs.trim().to_owned()),
                        params: Vec::new(),
                        enclosing_label_idx: None,
                        enclosing_macro_idx: None,
                    });
                }
            }
        }
        offset += raw_line.len();
    }
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '.')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$')
}

fn finalize_label_hierarchy_and_ranges(text: &str, defs: &mut [SymbolDef], refs: &mut [SymbolRef]) {
    // Ordinary labels sorted by start offset define ranges up to just before the next ordinary label
    let mut label_indices: Vec<usize> = defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == SymKind::Label)
        .map(|(i, _)| i)
        .collect();
    label_indices.sort_by_key(|&i| defs[i].name_span.0);

    for pos in 0..label_indices.len() {
        let idx = label_indices[pos];
        let start = defs[idx].name_span.0;
        let end = if pos + 1 < label_indices.len() {
            let next_start = defs[label_indices[pos + 1]].name_span.0;
            // End at the newline before the next label if possible
            trim_trailing_ws_before(text, start, next_start)
        } else {
            text.trim_end().len().max(start)
        };
        defs[idx].full_span = (start, end.max(defs[idx].name_span.1));
    }

    // Attach numeric labels to their enclosing ordinary label
    for i in 0..defs.len() {
        if defs[i].kind != SymKind::NumericLabel {
            continue;
        }
        let off = defs[i].name_span.0;
        let parent = label_indices
            .iter()
            .copied()
            .rfind(|&li| defs[li].full_span.0 <= off && off <= defs[li].full_span.1);
        defs[i].enclosing_label_idx = parent;
    }

    // Ensure enclosing_macro_idx on refs aligns with full_span
    let macro_ranges: Vec<(usize, usize, usize)> = defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == SymKind::Macro)
        .map(|(i, d)| (i, d.full_span.0, d.full_span.1))
        .collect();
    for r in refs {
        if r.enclosing_macro_idx.is_none() {
            r.enclosing_macro_idx = macro_ranges
                .iter()
                .find(|(_, s, e)| r.span.0 >= *s && r.span.0 <= *e)
                .map(|(i, _, _)| *i);
        }
    }
}

fn trim_trailing_ws_before(text: &str, min: usize, limit: usize) -> usize {
    let mut end = clamp_to_char_boundary(text, limit);
    while end > min {
        let prev = clamp_to_char_boundary(text, end - 1);
        let ch = text[prev..end].chars().next();
        if ch.is_some_and(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n') {
            end = prev;
        } else {
            break;
        }
    }
    end
}

fn line_text_at(text: &str, line_index: &LineIndex, byte_offset: usize) -> String {
    let pos = line_index.offset_to_position(text, byte_offset);
    let (start, end, _) = line_index.line_bounds(text, pos.line as usize);
    text.get(start..end).unwrap_or("").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_kernel_entry_symbols_and_hierarchy() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("root")
            .join("tests/fixtures");
        let mut idx = WorkspaceIndex::new(Some(fixtures.clone())).expect("index");
        let mut ctx = CompileContext::new(Some(fixtures.clone()), Default::default());

        let kernel_path = normalize_path(&fixtures.join("kernel_entry.S"));
        let kernel_uri = path_to_uri(&kernel_path).expect("uri");
        let file = idx
            .file_by_uri(&kernel_uri)
            .expect("kernel indexed")
            .clone();

        assert!(
            file.definitions
                .iter()
                .any(|d| d.kind == SymKind::Equ && d.name == "KERNEL_BASE")
        );
        assert!(
            file.definitions
                .iter()
                .any(|d| d.kind == SymKind::Macro && d.name == "SAVE_REG")
        );
        let start_label = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::Label && d.name == "_start")
            .expect("_start label");
        let trap_label = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::Label && d.name == "trap_vector")
            .expect("trap_vector label");
        assert!(start_label.full_span.1 < trap_label.name_span.0);

        let num_1 = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::NumericLabel && d.name == "1")
            .expect("numeric label 1");
        assert!(num_1.enclosing_label_idx.is_some());

        // Header #define resolved through #include "regs.h"
        let closure = idx.resolve_include_closure(&kernel_path, &mut ctx);
        let regs_path = normalize_path(&fixtures.join("regs.h"));
        assert!(closure.contains(&regs_path));
        let regs_file = idx.files.get(&regs_path).expect("regs.h indexed");
        assert!(
            regs_file
                .definitions
                .iter()
                .any(|d| d.kind == SymKind::Define && d.name == "MSTATUS_MIE")
        );
    }
}
