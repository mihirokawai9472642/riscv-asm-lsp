use std::collections::{HashMap, HashSet};

use lsp_types::{
    Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend,
};
use tree_sitter::Node;

use crate::context::CompileContext;
use crate::data::Database;
use crate::document::Document;
use crate::index::{SymKind, WorkspaceIndex, parse_macro_var_token, parse_numeric_ref_token};

pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::new("register"),
    SemanticTokenType::new("csr"),
    SemanticTokenType::new("label"),
    SemanticTokenType::new("constant"),
    SemanticTokenType::new("macro"),
    SemanticTokenType::new("parameter"),
];

pub const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[SemanticTokenModifier::DECLARATION];

const IDX_REGISTER: u32 = 0;
const IDX_CSR: u32 = 1;
const IDX_LABEL: u32 = 2;
const IDX_CONSTANT: u32 = 3;
const IDX_MACRO: u32 = 4;
const IDX_PARAMETER: u32 = 5;

const MOD_DECLARATION: u32 = 1 << 0;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

#[derive(Debug, Clone, Copy)]
struct RawToken {
    start_byte: usize,
    end_byte: usize,
    token_type: u32,
    modifiers: u32,
}

pub fn provide_semantic_tokens_full(
    doc: &Document,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
) -> Option<SemanticTokens> {
    collect_semantic_tokens(doc, db, ctx, index, None)
}

pub fn provide_semantic_tokens_range(
    doc: &Document,
    range: Range,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
) -> Option<SemanticTokens> {
    collect_semantic_tokens(doc, db, ctx, index, Some(range))
}

fn collect_semantic_tokens(
    doc: &Document,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
    range_filter: Option<Range>,
) -> Option<SemanticTokens> {
    let symbol_kinds = index.symbol_kinds_in_scope(&doc.uri, ctx);
    let mut raw = Vec::new();
    let mut decl_spans: HashSet<(usize, usize)> = HashSet::new();
    let mut local_non_csr_defs: HashSet<String> = HashSet::new();

    if let Some(file_idx) = index.file_by_uri(&doc.uri) {
        for def in &file_idx.definitions {
            if !matches!(def.kind, SymKind::Macro | SymKind::MacroParam) {
                local_non_csr_defs.insert(def.name.clone());
            }
            let (tok_type, is_macro) = match def.kind {
                SymKind::Label | SymKind::LocalLabel | SymKind::NumericLabel => (IDX_LABEL, false),
                SymKind::Equ | SymKind::Define => (IDX_CONSTANT, false),
                SymKind::Macro => (IDX_MACRO, true),
                SymKind::MacroParam => (IDX_PARAMETER, false),
            };
            if def.name_span.0 < def.name_span.1 {
                decl_spans.insert(def.name_span);
                raw.push(RawToken {
                    start_byte: def.name_span.0,
                    end_byte: def.name_span.1,
                    token_type: tok_type,
                    modifiers: MOD_DECLARATION,
                });
            }
            if is_macro {
                for p in &def.params {
                    if p.span.0 < p.span.1 {
                        decl_spans.insert(p.span);
                        raw.push(RawToken {
                            start_byte: p.span.0,
                            end_byte: p.span.1,
                            token_type: IDX_PARAMETER,
                            modifiers: MOD_DECLARATION,
                        });
                    }
                }
            }
        }
    }

    if let Some(tree) = &doc.tree {
        let mut walk_ctx = WalkCtx {
            text: &doc.text,
            db,
            symbol_kinds: &symbol_kinds,
            local_non_csr_defs: &local_non_csr_defs,
            decl_spans: &decl_spans,
            out: &mut raw,
        };
        walk_ast(tree.root_node(), false, &mut walk_ctx);
    }

    raw.sort_by_key(|t| (t.start_byte, t.end_byte, std::cmp::Reverse(t.modifiers)));

    let mut data = Vec::with_capacity(raw.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    let mut last_end_byte = 0usize;

    for tok in raw {
        if tok.start_byte < last_end_byte || tok.start_byte >= tok.end_byte {
            continue;
        }
        let span_range = doc
            .line_index
            .byte_span_to_range(&doc.text, tok.start_byte, tok.end_byte);
        // LSP semantic tokens must not span multiple lines
        if span_range.start.line != span_range.end.line {
            continue;
        }
        let length = span_range
            .end
            .character
            .saturating_sub(span_range.start.character);
        if length == 0 {
            continue;
        }
        if let Some(filter) = range_filter {
            if span_range.end.line < filter.start.line || span_range.start.line > filter.end.line {
                continue;
            }
            if span_range.end.line == filter.start.line
                && span_range.end.character <= filter.start.character
            {
                continue;
            }
            if span_range.start.line == filter.end.line
                && span_range.start.character >= filter.end.character
            {
                continue;
            }
        }

        let line = span_range.start.line;
        let start = span_range.start.character;
        let delta_line = line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            start.saturating_sub(prev_start)
        } else {
            start
        };

        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: tok.token_type,
            token_modifiers_bitset: tok.modifiers,
        });

        prev_line = line;
        prev_start = start;
        last_end_byte = tok.end_byte;
    }

    Some(SemanticTokens {
        result_id: None,
        data,
    })
}

struct WalkCtx<'a> {
    text: &'a str,
    db: &'a Database,
    symbol_kinds: &'a HashMap<String, SymKind>,
    local_non_csr_defs: &'a HashSet<String>,
    decl_spans: &'a HashSet<(usize, usize)>,
    out: &'a mut Vec<RawToken>,
}

fn walk_ast(node: Node<'_>, in_csr_stmt: bool, ctx: &mut WalkCtx<'_>) {
    let kind = node.kind();
    let span = (node.start_byte(), node.end_byte());

    match kind {
        "comment" | "string" | "char" => return,
        "register" => {
            if !ctx.decl_spans.contains(&span) {
                ctx.out.push(RawToken {
                    start_byte: span.0,
                    end_byte: span.1,
                    token_type: IDX_REGISTER,
                    modifiers: 0,
                });
            }
            return;
        }
        "numeric_label_reference" => {
            if !ctx.decl_spans.contains(&span) {
                ctx.out.push(RawToken {
                    start_byte: span.0,
                    end_byte: span.1,
                    token_type: IDX_LABEL,
                    modifiers: 0,
                });
            }
            return;
        }
        "macro_variable" | "string_macro_variable" => {
            if let Some(raw) = ctx.text.get(span.0..span.1)
                && let Some(param) = parse_macro_var_token(raw)
            {
                let end = (span.0 + 1 + param.len()).min(span.1);
                if !ctx.decl_spans.contains(&(span.0, end)) {
                    ctx.out.push(RawToken {
                        start_byte: span.0,
                        end_byte: end,
                        token_type: IDX_PARAMETER,
                        modifiers: 0,
                    });
                }
            }
            return;
        }
        "macro_parameter_name" => {
            if !ctx.decl_spans.contains(&span) {
                ctx.out.push(RawToken {
                    start_byte: span.0,
                    end_byte: span.1,
                    token_type: IDX_PARAMETER,
                    modifiers: MOD_DECLARATION,
                });
            }
            return;
        }
        "macro_name" => {
            if !ctx.decl_spans.contains(&span) {
                ctx.out.push(RawToken {
                    start_byte: span.0,
                    end_byte: span.1,
                    token_type: IDX_MACRO,
                    modifiers: 0,
                });
            }
            return;
        }
        "instruction" => {
            let mut next_csr_stmt = false;
            if let Some(mnem) = node.child_by_field_name("mnemonic")
                && let Some(name) = ctx.text.get(mnem.start_byte()..mnem.end_byte())
            {
                if let Some(inst) = ctx.db.instruction(name) {
                    next_csr_stmt = inst
                        .variants
                        .iter()
                        .any(|v| v.operands.iter().any(|o| o.class == "csr_sysreg"));
                } else if ctx.symbol_kinds.get(name) == Some(&SymKind::Macro) {
                    let mspan = (mnem.start_byte(), mnem.end_byte());
                    if !ctx.decl_spans.contains(&mspan) {
                        ctx.out.push(RawToken {
                            start_byte: mspan.0,
                            end_byte: mspan.1,
                            token_type: IDX_MACRO,
                            modifiers: 0,
                        });
                    }
                } else if let Some(param) = parse_macro_var_token(name) {
                    let start = mnem.start_byte();
                    let end = (start + 1 + param.len()).min(mnem.end_byte());
                    if !ctx.decl_spans.contains(&(start, end)) {
                        ctx.out.push(RawToken {
                            start_byte: start,
                            end_byte: end,
                            token_type: IDX_PARAMETER,
                            modifiers: 0,
                        });
                    }
                }
            }
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_ast(ops, next_csr_stmt, ctx);
            }
            return;
        }
        "call_statement" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Some(name) = ctx.text.get(name_node.start_byte()..name_node.end_byte())
                && !ctx
                    .decl_spans
                    .contains(&(name_node.start_byte(), name_node.end_byte()))
                && let Some(tok_type) = classify_symbol_name(name, false, ctx)
            {
                ctx.out.push(RawToken {
                    start_byte: name_node.start_byte(),
                    end_byte: name_node.end_byte(),
                    token_type: tok_type,
                    modifiers: 0,
                });
            }
            if let Some(ops) = node.child_by_field_name("operands") {
                walk_ast(ops, false, ctx);
            }
            return;
        }
        "symbol" => {
            if !ctx.decl_spans.contains(&span)
                && let Some(name) = ctx.text.get(span.0..span.1)
                && let Some(tok_type) = classify_symbol_name(name, in_csr_stmt, ctx)
            {
                ctx.out.push(RawToken {
                    start_byte: span.0,
                    end_byte: span.1,
                    token_type: tok_type,
                    modifiers: 0,
                });
            }
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ast(child, in_csr_stmt, ctx);
    }
}

fn classify_symbol_name(name: &str, in_csr_stmt: bool, ctx: &WalkCtx<'_>) -> Option<u32> {
    if parse_numeric_ref_token(name).is_some() {
        return Some(IDX_LABEL);
    }
    if ctx.db.register(name).is_some() {
        return Some(IDX_REGISTER);
    }
    if ctx.db.csr(name).is_some() && (in_csr_stmt || !ctx.local_non_csr_defs.contains(name)) {
        return Some(IDX_CSR);
    }
    match ctx.symbol_kinds.get(name)? {
        SymKind::Equ | SymKind::Define => Some(IDX_CONSTANT),
        SymKind::Macro => Some(IDX_MACRO),
        SymKind::Label | SymKind::LocalLabel | SymKind::NumericLabel => Some(IDX_LABEL),
        SymKind::MacroParam => Some(IDX_PARAMETER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use lsp_types::{Position, Uri};
    use std::str::FromStr;

    fn decode_tokens(text: &str, tokens: &[SemanticToken]) -> Vec<(String, u32, u32)> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        let mut line = 0u32;
        let mut col = 0u32;
        for tok in tokens {
            line += tok.delta_line;
            col = if tok.delta_line == 0 {
                col + tok.delta_start
            } else {
                tok.delta_start
            };
            let line_str = lines.get(line as usize).copied().unwrap_or("");
            let utf16: Vec<u16> = line_str.encode_utf16().collect();
            let s = col as usize;
            let e = (col + tok.length) as usize;
            let lexeme = utf16
                .get(s..e)
                .map(String::from_utf16_lossy)
                .unwrap_or_default();
            out.push((lexeme, tok.token_type, tok.token_modifiers_bitset));
        }
        out
    }

    #[test]
    fn emits_expected_token_types_and_multibyte_positions() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = Uri::from_str("file:///sem.S").expect("uri");
        let text = "\
# 日本語コメント 🚀
.equ BASE, 0x100
.macro SAVE reg
    sd \\reg, 0(sp)
.endm
_start:
    csrw stvec, t0
    li t1, BASE
    SAVE ra
1:
    j 1b
    j _start
";
        let doc = store.open(uri, 1, text.to_owned());
        let mut ctx = CompileContext::new(None, Config::default());
        let mut index = WorkspaceIndex::new(None).expect("index");
        index.upsert_open_document(doc);

        let full = provide_semantic_tokens_full(doc, &db, &mut ctx, &mut index).expect("tokens");
        let decoded = decode_tokens(text, &full.data);

        assert!(decoded.contains(&("BASE".to_owned(), IDX_CONSTANT, MOD_DECLARATION)));
        assert!(decoded.contains(&("SAVE".to_owned(), IDX_MACRO, MOD_DECLARATION)));
        assert!(decoded.contains(&("reg".to_owned(), IDX_PARAMETER, MOD_DECLARATION)));
        assert!(decoded.contains(&("\\reg".to_owned(), IDX_PARAMETER, 0)));
        assert!(decoded.contains(&("sp".to_owned(), IDX_REGISTER, 0)));
        assert!(decoded.contains(&("_start".to_owned(), IDX_LABEL, MOD_DECLARATION)));
        assert!(decoded.contains(&("stvec".to_owned(), IDX_CSR, 0)));
        assert!(decoded.contains(&("t0".to_owned(), IDX_REGISTER, 0)));
        assert!(decoded.contains(&("BASE".to_owned(), IDX_CONSTANT, 0)));
        assert!(decoded.contains(&("SAVE".to_owned(), IDX_MACRO, 0)));
        assert!(decoded.contains(&("1".to_owned(), IDX_LABEL, MOD_DECLARATION)));
        assert!(decoded.contains(&("1b".to_owned(), IDX_LABEL, 0)));
        assert!(decoded.contains(&("_start".to_owned(), IDX_LABEL, 0)));

        // Range filter covering only lines 6..=7
        let range = Range::new(Position::new(6, 0), Position::new(7, 20));
        let ranged = provide_semantic_tokens_range(doc, range, &db, &mut ctx, &mut index)
            .expect("range tokens");
        let range_decoded = decode_tokens(text, &ranged.data);
        assert!(range_decoded.contains(&("stvec".to_owned(), IDX_CSR, 0)));
        assert!(range_decoded.contains(&("BASE".to_owned(), IDX_CONSTANT, 0)));
        assert!(!range_decoded.contains(&("_start".to_owned(), IDX_LABEL, MOD_DECLARATION)));
    }
}
