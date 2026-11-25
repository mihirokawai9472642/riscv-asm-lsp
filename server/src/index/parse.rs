use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lsp_types::Uri;
use tree_sitter::{Node, Parser};

use crate::ast::{AstVisitor, walk_node, walk_node_after_offset};
use crate::position::{LineIndex, clamp_to_char_boundary};
use crate::preprocessor::{
    parse_cpp_define_line as parse_cpp_define_raw, parse_cpp_include_line as parse_cpp_include_raw,
};

use super::{
    AssignKind, FileIndex, IncludeDirective, MacroParamDef, RefKind, RepeatScope, SymKind,
    SymbolDef, SymbolRef,
};

pub(super) fn is_header_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("h" | "H" | "hpp")
    )
}

pub(super) fn is_eager_asm_path(path: &Path) -> bool {
    if crate::context::is_linker_script_template(path) {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("S" | "s" | "sx" | "inc")
    )
}

pub(super) fn resolve_include_path(
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

pub(super) struct BuildFileInput<'a> {
    pub path: PathBuf,
    pub uri: Uri,
    pub is_header: bool,
    pub preprocessed: bool,
    pub text: String,
    pub line_index: LineIndex,
    pub existing_tree: Option<&'a tree_sitter::Tree>,
}

pub(super) fn build_file_index(input: BuildFileInput<'_>, parser: &mut Parser) -> FileIndex {
    if input.is_header {
        return parse_header_file(input.path, input.uri, input.text, input.line_index);
    }

    let owned_tree = if input.existing_tree.is_none() {
        parser.parse(&input.text, None)
    } else {
        None
    };
    let tree = input.existing_tree.or(owned_tree.as_ref());

    let mut definitions = Vec::new();
    let mut repeat_scopes = Vec::new();
    let mut references = Vec::new();
    let mut includes = Vec::new();
    let mut globals = HashSet::new();

    if input.preprocessed {
        // Scan preprocessor `#include` and `#define` lines (tree-sitter treats `#...` as comment).
        scan_cpp_includes(&input.text, &mut includes);
        scan_cpp_defines(&input.text, &mut definitions);
    }

    if let Some(tree) = tree {
        let mut ctx = ExtractVisitor {
            text: &input.text,
            line_index: &input.line_index,
            definitions: &mut definitions,
            repeat_scopes: &mut repeat_scopes,
            references: &mut references,
            includes: &mut includes,
            globals: &mut globals,
            macro_stack: Vec::new(),
            repeat_stack: Vec::new(),
        };
        walk_node(tree.root_node(), &input.text, &mut ctx);
        for open_idx in ctx.macro_stack {
            if let Some(def) = ctx.definitions.get_mut(open_idx) {
                def.full_span.1 = input.text.len();
            }
        }
        for open_rep in ctx.repeat_stack.into_iter().flatten() {
            if let Some(scope) = ctx.repeat_scopes.get_mut(open_rep) {
                scope.full_span.1 = input.text.len();
            }
        }
    }

    scan_fallback_assignments(&input.text, &input.line_index, &mut definitions);
    finalize_label_hierarchy_and_ranges(&input.text, &mut definitions, &mut references);

    FileIndex {
        path: input.path,
        uri: input.uri,
        is_header: false,
        preprocessed: input.preprocessed,
        text: input.text,
        line_index: input.line_index,
        definitions,
        repeat_scopes,
        references,
        includes,
        globals,
    }
}

fn parse_header_file(path: PathBuf, uri: Uri, text: String, line_index: LineIndex) -> FileIndex {
    let mut definitions = Vec::new();
    let mut includes = Vec::new();
    scan_cpp_includes(&text, &mut includes);
    scan_cpp_defines(&text, &mut definitions);

    FileIndex {
        path,
        uri,
        is_header: true,
        preprocessed: true,
        text,
        line_index,
        definitions,
        repeat_scopes: Vec::new(),
        references: Vec::new(),
        includes,
        globals: HashSet::new(),
    }
}

fn scan_cpp_defines(text: &str, definitions: &mut Vec<SymbolDef>) {
    let mut line_start = 0usize;
    let mut in_block = false;
    for raw_line in text.split_inclusive('\n') {
        let line_end = line_start + raw_line.len();
        let started_in_block = in_block;
        let stripped = crate::line::strip_comments_with_state(raw_line, &mut in_block, true);
        if let Some(parsed) = parse_cpp_define_raw(&stripped) {
            let line_text = if started_in_block {
                stripped.trim().to_owned()
            } else {
                raw_line.trim().to_owned()
            };
            definitions.push(SymbolDef {
                name: parsed.name.to_owned(),
                kind: SymKind::Define,
                assign_kind: None,
                name_span: (
                    line_start + parsed.name_span.0,
                    line_start + parsed.name_span.1,
                ),
                full_span: (line_start, line_end),
                line_text,
                value_text: parsed.value,
                params: Vec::new(),
                enclosing_label_idx: None,
                enclosing_macro_idx: None,
            });
        }
        line_start = line_end;
    }
}

fn scan_cpp_includes(text: &str, out: &mut Vec<IncludeDirective>) {
    let mut in_block = false;
    for line in text.lines() {
        let stripped = crate::line::strip_comments_with_state(line, &mut in_block, true);
        if let Some(inc) = parse_cpp_include_line(&stripped) {
            out.push(inc);
        }
    }
}

pub(super) fn parse_asm_include_line(trimmed: &str) -> Option<IncludeDirective> {
    let rest = trimmed.strip_prefix(".include")?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(IncludeDirective {
        target: inner[..end].to_owned(),
        is_system: false,
    })
}

pub(super) fn parse_cpp_include_line(line: &str) -> Option<IncludeDirective> {
    parse_cpp_include_raw(line).map(|(target, is_system)| IncludeDirective { target, is_system })
}

struct ExtractVisitor<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
    definitions: &'a mut Vec<SymbolDef>,
    repeat_scopes: &'a mut Vec<RepeatScope>,
    references: &'a mut Vec<SymbolRef>,
    includes: &'a mut Vec<IncludeDirective>,
    globals: &'a mut HashSet<String>,
    macro_stack: Vec<usize>,
    repeat_stack: Vec<Option<usize>>,
}

impl ExtractVisitor<'_> {
    fn cur_macro(&self) -> Option<usize> {
        self.macro_stack.last().copied()
    }

    fn push_ref(&mut self, name: &str, span: (usize, usize)) {
        if let Some((digits, forward)) = super::parse_numeric_ref_token(name) {
            self.references.push(SymbolRef {
                name: digits.to_owned(),
                kind: if forward {
                    RefKind::NumericForward
                } else {
                    RefKind::NumericBackward
                },
                span,
                enclosing_macro_idx: self.cur_macro(),
            });
        } else if name != "." {
            self.references.push(SymbolRef {
                name: name.to_owned(),
                kind: RefKind::Identifier,
                span,
                enclosing_macro_idx: self.cur_macro(),
            });
        }
    }

    fn handle_macro_directive(&mut self, node: Node<'_>) {
        if let Some(name_node) = node.child_by_field_name("name")
            && let Some(name) = self.text.get(name_node.start_byte()..name_node.end_byte())
        {
            let mut params = Vec::new();
            if let Some(params_node) = node.child_by_field_name("parameters") {
                collect_macro_params(params_node, self.text, &mut params);
            }
            let def_idx = self.definitions.len();
            self.definitions.push(SymbolDef {
                name: name.to_owned(),
                kind: SymKind::Macro,
                assign_kind: None,
                name_span: (name_node.start_byte(), name_node.end_byte()),
                full_span: (node.start_byte(), self.text.len()),
                line_text: line_text_at(self.text, self.line_index, node.start_byte()),
                value_text: None,
                params,
                enclosing_label_idx: None,
                enclosing_macro_idx: None,
            });
            self.macro_stack.push(def_idx);
        }
    }

    fn handle_irp_directive(&mut self, node: Node<'_>, mnem: String) {
        if let Some(ops_node) = node.child_by_field_name("operands")
            && let Some(raw_ops) = self.text.get(ops_node.start_byte()..ops_node.end_byte())
        {
            let lead = raw_ops.len() - raw_ops.trim_start().len();
            let param_start = ops_node.start_byte() + lead;
            let trimmed = &raw_ops[lead..];
            let param_len = trimmed
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$'))
                .count();
            if param_len > 0 {
                let param_name = &trimmed[..param_len];
                let param_end = param_start + param_len;
                let scope_idx = self.repeat_scopes.len();
                self.repeat_scopes.push(RepeatScope {
                    directive: mnem,
                    param: MacroParamDef {
                        name: param_name.to_owned(),
                        span: (param_start, param_end),
                    },
                    full_span: (node.start_byte(), self.text.len()),
                    line_text: line_text_at(self.text, self.line_index, node.start_byte()),
                });
                self.repeat_stack.push(Some(scope_idx));
                walk_node_after_offset(ops_node, param_end, self.text, self);
                return;
            }
        }
        self.repeat_stack.push(None);
    }

    fn handle_equ_directive(&mut self, node: Node<'_>, mnem: &str) -> bool {
        let Some(ops_node) = node.child_by_field_name("operands") else {
            return true;
        };
        let mut cursor = ops_node.walk();
        let children: Vec<Node<'_>> = ops_node.children(&mut cursor).collect();
        let Some(sym_node) = children.iter().find(|c| c.kind() == "symbol") else {
            return true;
        };
        let Some(name) = self.text.get(sym_node.start_byte()..sym_node.end_byte()) else {
            return true;
        };
        if name == "." {
            return true;
        }
        let sym_id = sym_node.id();
        let value_text = self
            .text
            .get(sym_node.end_byte()..ops_node.end_byte())
            .and_then(|rest| rest.split_once(',').map(|(_, v)| v.trim().to_owned()))
            .filter(|s| !s.is_empty());
        let assign_kind = match mnem {
            ".set" => AssignKind::Set,
            ".equiv" => AssignKind::Equiv,
            _ => AssignKind::Equ,
        };
        self.definitions.push(SymbolDef {
            name: name.to_owned(),
            kind: SymKind::Equ,
            assign_kind: Some(assign_kind),
            name_span: (sym_node.start_byte(), sym_node.end_byte()),
            full_span: (node.start_byte(), node.end_byte()),
            line_text: line_text_at(self.text, self.line_index, node.start_byte()),
            value_text,
            params: Vec::new(),
            enclosing_label_idx: None,
            enclosing_macro_idx: None,
        });
        for child in children {
            if child.id() != sym_id {
                walk_node(child, self.text, self);
            }
        }
        false
    }
}

impl AstVisitor for ExtractVisitor<'_> {
    fn on_label(&mut self, node: Node<'_>, name: &str) {
        let name_start = node.start_byte();
        let name_end = name_start + name.len();
        let sym_kind = if name.bytes().all(|b| b.is_ascii_digit()) {
            SymKind::NumericLabel
        } else if name.starts_with(".L") {
            SymKind::LocalLabel
        } else {
            SymKind::Label
        };
        self.definitions.push(SymbolDef {
            name: name.to_owned(),
            kind: sym_kind,
            assign_kind: None,
            name_span: (name_start, name_end),
            full_span: (node.start_byte(), node.end_byte()),
            line_text: line_text_at(self.text, self.line_index, name_start),
            value_text: None,
            params: Vec::new(),
            enclosing_label_idx: None,
            enclosing_macro_idx: None,
        });
    }

    fn on_numeric_label(&mut self, node: Node<'_>, name: &str) {
        let name_start = node.start_byte();
        let name_end = name_start + name.len();
        self.definitions.push(SymbolDef {
            name: name.to_owned(),
            kind: SymKind::NumericLabel,
            assign_kind: None,
            name_span: (name_start, name_end),
            full_span: (node.start_byte(), node.end_byte()),
            line_text: line_text_at(self.text, self.line_index, name_start),
            value_text: None,
            params: Vec::new(),
            enclosing_label_idx: None,
            enclosing_macro_idx: None,
        });
    }

    fn enter_instruction(&mut self, mnem: Node<'_>, name: &str) -> bool {
        let span = (mnem.start_byte(), mnem.end_byte());
        if let Some(param_name) = super::parse_macro_var_token(name) {
            self.references.push(SymbolRef {
                name: param_name.to_owned(),
                kind: RefKind::MacroParam,
                span,
                enclosing_macro_idx: self.cur_macro(),
            });
        } else if name != "." {
            self.references.push(SymbolRef {
                name: name.to_owned(),
                kind: RefKind::Identifier,
                span,
                enclosing_macro_idx: self.cur_macro(),
            });
        }
        false
    }

    fn enter_call_statement(&mut self, name_node: Node<'_>, name: &str) -> bool {
        self.references.push(SymbolRef {
            name: name.to_owned(),
            kind: RefKind::Identifier,
            span: (name_node.start_byte(), name_node.end_byte()),
            enclosing_macro_idx: self.cur_macro(),
        });
        false
    }

    fn on_assignment(
        &mut self,
        full: Node<'_>,
        left: Node<'_>,
        name: &str,
        right: Option<Node<'_>>,
    ) {
        let value_text = right
            .and_then(|r| self.text.get(r.start_byte()..r.end_byte()))
            .map(|s| s.trim().to_owned());
        self.definitions.push(SymbolDef {
            name: name.to_owned(),
            kind: SymKind::Equ,
            assign_kind: Some(AssignKind::Equals),
            name_span: (left.start_byte(), left.end_byte()),
            full_span: (full.start_byte(), full.end_byte()),
            line_text: line_text_at(self.text, self.line_index, left.start_byte()),
            value_text,
            params: Vec::new(),
            enclosing_label_idx: None,
            enclosing_macro_idx: None,
        });
    }

    fn on_symbol(&mut self, node: Node<'_>, name: &str) {
        self.push_ref(name, (node.start_byte(), node.end_byte()));
    }

    fn on_numeric_label_ref(&mut self, node: Node<'_>, raw: &str) {
        if let Some((digits, forward)) = super::parse_numeric_ref_token(raw) {
            self.references.push(SymbolRef {
                name: digits.to_owned(),
                kind: if forward {
                    RefKind::NumericForward
                } else {
                    RefKind::NumericBackward
                },
                span: (node.start_byte(), node.end_byte()),
                enclosing_macro_idx: self.cur_macro(),
            });
        }
    }

    fn on_macro_var(&mut self, param: &str, span: (usize, usize)) {
        self.references.push(SymbolRef {
            name: param.to_owned(),
            kind: RefKind::MacroParam,
            span,
            enclosing_macro_idx: self.cur_macro(),
        });
    }

    fn on_directive(&mut self, node: Node<'_>) -> bool {
        let Some(mnem_node) = node.child_by_field_name("mnemonic") else {
            return true;
        };
        let Some(mnem_raw) = self.text.get(mnem_node.start_byte()..mnem_node.end_byte()) else {
            return true;
        };
        let mnem = mnem_raw.to_ascii_lowercase();
        match mnem.as_str() {
            ".macro" => {
                self.handle_macro_directive(node);
                false
            }
            ".endm" => {
                if let Some(open_idx) = self.macro_stack.pop()
                    && let Some(def) = self.definitions.get_mut(open_idx)
                {
                    def.full_span.1 = node.end_byte();
                }
                false
            }
            ".irp" | ".irpc" => {
                self.handle_irp_directive(node, mnem);
                false
            }
            ".rept" => {
                self.repeat_stack.push(None);
                true
            }
            ".endr" => {
                if let Some(Some(scope_idx)) = self.repeat_stack.pop()
                    && let Some(scope) = self.repeat_scopes.get_mut(scope_idx)
                {
                    scope.full_span.1 = node.end_byte();
                }
                false
            }
            ".equ" | ".set" | ".equiv" => self.handle_equ_directive(node, &mnem),
            ".include" => {
                if let Some(ops_node) = node.child_by_field_name("operands")
                    && let Some(raw) = self.text.get(ops_node.start_byte()..ops_node.end_byte())
                {
                    let trimmed = raw.trim();
                    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                    {
                        self.includes.push(IncludeDirective {
                            target: inner.to_owned(),
                            is_system: false,
                        });
                    }
                }
                false
            }
            ".globl" | ".global" => {
                if let Some(ops_node) = node.child_by_field_name("operands") {
                    let mut cursor = ops_node.walk();
                    for child in ops_node.children(&mut cursor) {
                        if child.kind() == "symbol"
                            && let Some(name) = self.text.get(child.start_byte()..child.end_byte())
                        {
                            self.globals.insert(name.to_owned());
                        }
                    }
                }
                true
            }
            _ => true,
        }
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
    let mut in_block = false;
    for raw_line in text.split_inclusive('\n') {
        let code = crate::line::strip_comments_with_state(raw_line, &mut in_block, false);
        if let Some((lhs, rhs)) = code.split_once('=') {
            let lhs_trim = lhs.trim();
            if !lhs_trim.is_empty()
                && !lhs_trim.ends_with(['!', '<', '>'])
                && !rhs.starts_with('=')
                && is_valid_ident(lhs_trim)
                && let Some(rel) = code.find(lhs_trim)
            {
                let name_start = offset + rel;
                if existing_equs.insert(name_start) {
                    defs.push(SymbolDef {
                        name: lhs_trim.to_owned(),
                        kind: SymKind::Equ,
                        assign_kind: Some(AssignKind::Equals),
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
    if s == "." {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '.')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$')
}

fn finalize_label_hierarchy_and_ranges(text: &str, defs: &mut [SymbolDef], refs: &mut [SymbolRef]) {
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
            trim_trailing_ws_before(text, start, next_start)
        } else {
            text.trim_end().len().max(start)
        };
        defs[idx].full_span = (start, end.max(defs[idx].name_span.1));
    }

    for i in 0..defs.len() {
        if !matches!(defs[i].kind, SymKind::NumericLabel | SymKind::LocalLabel) {
            continue;
        }
        let off = defs[i].name_span.0;
        let idx_pos = label_indices.partition_point(|&li| defs[li].full_span.0 <= off);
        let parent = idx_pos.checked_sub(1).and_then(|k| {
            let li = label_indices[k];
            (off <= defs[li].full_span.1).then_some(li)
        });
        defs[i].enclosing_label_idx = parent;
    }

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
    let line = line_index.line_at_offset(byte_offset);
    let (start, end, _) = line_index.line_bounds(text, line);
    text.get(start..end).unwrap_or("").trim().to_owned()
}
