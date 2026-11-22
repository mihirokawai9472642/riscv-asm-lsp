use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lsp_types::Uri;
use tree_sitter::{Node, Parser};

use crate::position::{LineIndex, clamp_to_char_boundary};

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
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("S" | "s" | "inc")
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

pub(super) fn build_file_index(
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
    let mut repeat_scopes = Vec::new();
    let mut references = Vec::new();
    let mut includes = Vec::new();
    let mut globals = HashSet::new();

    // Scan preprocessor `#include` and `#define` lines (tree-sitter treats `#...` as comment).
    scan_cpp_includes(&text, &mut includes);
    scan_cpp_defines(&text, &mut definitions);

    if let Some(tree) = tree {
        let mut ctx = ExtractCtx {
            text: &text,
            line_index: &line_index,
            definitions: &mut definitions,
            repeat_scopes: &mut repeat_scopes,
            references: &mut references,
            includes: &mut includes,
            globals: &mut globals,
            macro_stack: Vec::new(),
            repeat_stack: Vec::new(),
        };
        walk_ast(tree.root_node(), &mut ctx);
        // Close any unclosed macros cleanly at EOF
        for open_idx in ctx.macro_stack {
            if let Some(def) = ctx.definitions.get_mut(open_idx) {
                def.full_span.1 = text.len();
            }
        }
        for open_rep in ctx.repeat_stack.into_iter().flatten() {
            if let Some(scope) = ctx.repeat_scopes.get_mut(open_rep) {
                scope.full_span.1 = text.len();
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
    for raw_line in text.split_inclusive('\n') {
        let line_end = line_start + raw_line.len();
        let trimmed = raw_line.trim();
        if let Some(rest) = trimmed.strip_prefix('#')
            && let Some(after_def) = rest.trim_start().strip_prefix("define")
            && after_def.chars().next().is_some_and(char::is_whitespace)
        {
            let after_ws = after_def.trim_start();
            let ident: String = after_ws
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let after_ident = &after_ws[ident.len()..];
            if !ident.is_empty()
                && (after_ident.is_empty()
                    || after_ident.starts_with(char::is_whitespace)
                    || after_ident.starts_with('('))
                && let Some(rel) = raw_line.find(&ident)
            {
                let name_start = line_start + rel;
                let name_end = name_start + ident.len();
                let raw_val = if after_ident.starts_with('(') {
                    after_ident.trim()
                } else {
                    strip_cpp_line_comment(after_ident).trim()
                };
                let value_text = (!raw_val.is_empty()).then(|| raw_val.to_owned());
                definitions.push(SymbolDef {
                    name: ident,
                    kind: SymKind::Define,
                    assign_kind: None,
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
}

fn strip_cpp_line_comment(s: &str) -> &str {
    if let Some(idx) = s.find("//") {
        &s[..idx]
    } else if let Some(idx) = s.find("/*") {
        &s[..idx]
    } else {
        s
    }
}

fn scan_cpp_includes(text: &str, out: &mut Vec<IncludeDirective>) {
    for line in text.lines() {
        if let Some(inc) = parse_cpp_include_line(line.trim()) {
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

pub(super) fn parse_cpp_include_line(trimmed: &str) -> Option<IncludeDirective> {
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
    repeat_scopes: &'a mut Vec<RepeatScope>,
    references: &'a mut Vec<SymbolRef>,
    includes: &'a mut Vec<IncludeDirective>,
    globals: &'a mut HashSet<String>,
    macro_stack: Vec<usize>,
    repeat_stack: Vec<Option<usize>>,
}

fn walk_ast_after_offset(node: Node<'_>, min_start: usize, ctx: &mut ExtractCtx<'_>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.end_byte() <= min_start {
            continue;
        }
        if child.start_byte() >= min_start {
            walk_ast(child, ctx);
        } else {
            walk_ast_after_offset(child, min_start, ctx);
        }
    }
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
                        assign_kind: None,
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
                        assign_kind: None,
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
                if let Some(param_name) = super::parse_macro_var_token(name) {
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
                    assign_kind: Some(AssignKind::Equals),
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
                if let Some((digits, forward)) = super::parse_numeric_ref_token(name) {
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
                && let Some((digits, forward)) = super::parse_numeric_ref_token(text)
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
                && let Some(param) = super::parse_macro_var_token(raw)
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
                assign_kind: None,
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

    if matches!(mnem.as_str(), ".irp" | ".irpc") {
        if let Some(ops_node) = node.child_by_field_name("operands")
            && let Some(raw_ops) = ctx.text.get(ops_node.start_byte()..ops_node.end_byte())
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
                let scope_idx = ctx.repeat_scopes.len();
                ctx.repeat_scopes.push(RepeatScope {
                    directive: mnem,
                    param: MacroParamDef {
                        name: param_name.to_owned(),
                        span: (param_start, param_end),
                    },
                    full_span: (node.start_byte(), ctx.text.len()),
                    line_text: line_text_at(ctx.text, ctx.line_index, node.start_byte()),
                });
                ctx.repeat_stack.push(Some(scope_idx));
                walk_ast_after_offset(ops_node, param_end, ctx);
                return;
            }
        }
        ctx.repeat_stack.push(None);
        return;
    }

    if mnem == ".rept" {
        ctx.repeat_stack.push(None);
        if let Some(ops_node) = node.child_by_field_name("operands") {
            walk_ast(ops_node, ctx);
        }
        return;
    }

    if mnem == ".endr" {
        if let Some(Some(scope_idx)) = ctx.repeat_stack.pop()
            && let Some(scope) = ctx.repeat_scopes.get_mut(scope_idx)
        {
            scope.full_span.1 = node.end_byte();
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
            let assign_kind = match mnem.as_str() {
                ".set" => AssignKind::Set,
                ".equiv" => AssignKind::Equiv,
                _ => AssignKind::Equ,
            };
            ctx.definitions.push(SymbolDef {
                name: name.to_owned(),
                kind: SymKind::Equ,
                assign_kind: Some(assign_kind),
                name_span: (sym_node.start_byte(), sym_node.end_byte()),
                full_span: (node.start_byte(), node.end_byte()),
                line_text: line_text_at(ctx.text, ctx.line_index, node.start_byte()),
                value_text,
                params: Vec::new(),
                enclosing_label_idx: None,
                enclosing_macro_idx: None,
            });
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
            trim_trailing_ws_before(text, start, next_start)
        } else {
            text.trim_end().len().max(start)
        };
        defs[idx].full_span = (start, end.max(defs[idx].name_span.1));
    }

    // Attach numeric labels to their enclosing ordinary label via binary search (O(log N))
    for i in 0..defs.len() {
        if defs[i].kind != SymKind::NumericLabel {
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
    let line = line_index.line_at_offset(byte_offset);
    let (start, end, _) = line_index.line_bounds(text, line);
    text.get(start..end).unwrap_or("").trim().to_owned()
}
