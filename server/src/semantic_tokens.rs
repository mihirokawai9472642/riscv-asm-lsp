use std::collections::{HashMap, HashSet};

use lsp_types::{
    Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend,
};
use tree_sitter::Node;

use crate::ast::{AstVisitor, walk_node};
use crate::context::CompileContext;
use crate::data::Database;
use crate::document::Document;
use crate::index::{SymKind, WorkspaceIndex, parse_macro_var_token, parse_numeric_ref_token};
use crate::preprocessor::{HashLineKind, classify_hash_line};

pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::new("register"),
    SemanticTokenType::new("csr"),
    SemanticTokenType::new("label"),
    SemanticTokenType::new("constant"),
    SemanticTokenType::new("macro"),
    SemanticTokenType::new("parameter"),
    SemanticTokenType::new("preprocessor"),
    SemanticTokenType::COMMENT,
];

pub const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[SemanticTokenModifier::DECLARATION];

const IDX_REGISTER: u32 = 0;
const IDX_CSR: u32 = 1;
const IDX_LABEL: u32 = 2;
const IDX_CONSTANT: u32 = 3;
const IDX_MACRO: u32 = 4;
const IDX_PARAMETER: u32 = 5;
const IDX_PREPROCESSOR: u32 = 6;
const IDX_COMMENT: u32 = 7;

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
    let file_ctx = ctx.for_uri(&doc.uri);
    index.sync_preprocessed(&doc.uri, file_ctx.preprocessed);
    let symbol_kinds = index.symbol_kinds_in_scope(&doc.uri, ctx);
    let mut raw = Vec::new();
    let mut decl_spans = HashSet::new();
    let mut local_non_csr_defs = HashSet::new();

    collect_hash_line_tokens(doc, file_ctx.preprocessed, &mut raw);
    collect_declaration_tokens(
        doc,
        index,
        &mut decl_spans,
        &mut local_non_csr_defs,
        &mut raw,
    );

    if let Some(tree) = &doc.tree {
        let mut visitor = TokenVisitor {
            db,
            symbol_kinds: &symbol_kinds,
            local_non_csr_defs: &local_non_csr_defs,
            decl_spans: &decl_spans,
            in_csr_stmt: false,
            out: &mut raw,
        };
        walk_node(tree.root_node(), &doc.text, &mut visitor);
    }

    Some(SemanticTokens {
        result_id: None,
        data: encode_delta_tokens(doc, raw, range_filter),
    })
}

fn collect_hash_line_tokens(doc: &Document, preprocessed: bool, raw: &mut Vec<RawToken>) {
    let mut in_block = false;
    for line_idx in 0..doc.line_index.line_count() {
        let (ls, ce, _) = doc.line_index.line_bounds(&doc.text, line_idx);
        let line = doc.text.get(ls..ce).unwrap_or("");
        let started_in_block = in_block;
        let _ = crate::line::strip_comments_with_state(line, &mut in_block, preprocessed);
        if started_in_block {
            continue;
        }
        let Some(kind) = classify_hash_line(line) else {
            continue;
        };
        if preprocessed {
            let (start, end) = match kind {
                HashLineKind::Directive {
                    hash_offset,
                    name_span,
                    ..
                } => (ls + hash_offset, ls + name_span.1),
                HashLineKind::LineMarker {
                    hash_offset,
                    num_span,
                } => (ls + hash_offset, ls + num_span.1),
                HashLineKind::NullDirective { hash_offset } => {
                    (ls + hash_offset, ls + hash_offset + 1)
                }
                HashLineKind::Comment { .. } => continue,
            };
            raw.push(RawToken {
                start_byte: start,
                end_byte: end,
                token_type: IDX_PREPROCESSOR,
                modifiers: 0,
            });
        } else {
            raw.push(RawToken {
                start_byte: ls + kind.hash_offset(),
                end_byte: ce,
                token_type: IDX_COMMENT,
                modifiers: 0,
            });
        }
    }
}

fn collect_declaration_tokens(
    doc: &Document,
    index: &WorkspaceIndex,
    decl_spans: &mut HashSet<(usize, usize)>,
    local_non_csr_defs: &mut HashSet<String>,
    raw: &mut Vec<RawToken>,
) {
    let Some(file_idx) = index.file_by_uri(&doc.uri) else {
        return;
    };
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
    for scope in &file_idx.repeat_scopes {
        if scope.param.span.0 < scope.param.span.1 {
            decl_spans.insert(scope.param.span);
            raw.push(RawToken {
                start_byte: scope.param.span.0,
                end_byte: scope.param.span.1,
                token_type: IDX_PARAMETER,
                modifiers: MOD_DECLARATION,
            });
        }
    }
}

fn encode_delta_tokens(
    doc: &Document,
    mut raw: Vec<RawToken>,
    range_filter: Option<Range>,
) -> Vec<SemanticToken> {
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
        if span_range.start.line != span_range.end.line {
            continue;
        }
        let length = span_range
            .end
            .character
            .saturating_sub(span_range.start.character);
        if length == 0 || !range_includes(range_filter, span_range) {
            continue;
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

    data
}

fn range_includes(filter: Option<Range>, span: Range) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if span.end.line < filter.start.line || span.start.line > filter.end.line {
        return false;
    }
    if span.end.line == filter.start.line && span.end.character <= filter.start.character {
        return false;
    }
    if span.start.line == filter.end.line && span.start.character >= filter.end.character {
        return false;
    }
    true
}

struct TokenVisitor<'a> {
    db: &'a Database,
    symbol_kinds: &'a HashMap<String, SymKind>,
    local_non_csr_defs: &'a HashSet<String>,
    decl_spans: &'a HashSet<(usize, usize)>,
    in_csr_stmt: bool,
    out: &'a mut Vec<RawToken>,
}

impl TokenVisitor<'_> {
    fn push_if_undeclared(&mut self, span: (usize, usize), token_type: u32, modifiers: u32) {
        if !self.decl_spans.contains(&span) {
            self.out.push(RawToken {
                start_byte: span.0,
                end_byte: span.1,
                token_type,
                modifiers,
            });
        }
    }

    fn classify_symbol_name(&self, name: &str) -> Option<u32> {
        if name == "." {
            return None;
        }
        if parse_numeric_ref_token(name).is_some() {
            return Some(IDX_LABEL);
        }
        if self.db.register(name).is_some() {
            return Some(IDX_REGISTER);
        }
        if self.db.csr(name).is_some()
            && (self.in_csr_stmt
                || (!self.local_non_csr_defs.contains(name)
                    && !self.symbol_kinds.contains_key(name)))
        {
            return Some(IDX_CSR);
        }
        match self.symbol_kinds.get(name)? {
            SymKind::Equ | SymKind::Define => Some(IDX_CONSTANT),
            SymKind::Macro => Some(IDX_MACRO),
            SymKind::Label | SymKind::LocalLabel | SymKind::NumericLabel => Some(IDX_LABEL),
            SymKind::MacroParam => Some(IDX_PARAMETER),
        }
    }
}

impl AstVisitor for TokenVisitor<'_> {
    fn enter_instruction(&mut self, mnem: Node<'_>, name: &str) -> bool {
        let prev = self.in_csr_stmt;
        let mspan = (mnem.start_byte(), mnem.end_byte());
        if let Some(inst) = self.db.instruction(name) {
            self.in_csr_stmt = inst
                .variants
                .iter()
                .any(|v| v.operands.iter().any(|o| o.class == "csr_sysreg"));
        } else if self.symbol_kinds.get(name) == Some(&SymKind::Macro) {
            self.in_csr_stmt = false;
            self.push_if_undeclared(mspan, IDX_MACRO, 0);
        } else if let Some(param) = parse_macro_var_token(name) {
            self.in_csr_stmt = false;
            let end = (mspan.0 + 1 + param.len()).min(mspan.1);
            self.push_if_undeclared((mspan.0, end), IDX_PARAMETER, 0);
        } else {
            self.in_csr_stmt = false;
        }
        prev
    }

    fn leave_instruction(&mut self, prev_state: bool) {
        self.in_csr_stmt = prev_state;
    }

    fn enter_call_statement(&mut self, name_node: Node<'_>, name: &str) -> bool {
        let prev = self.in_csr_stmt;
        self.in_csr_stmt = false;
        let span = (name_node.start_byte(), name_node.end_byte());
        if !self.decl_spans.contains(&span)
            && let Some(tok_type) = self.classify_symbol_name(name)
        {
            self.out.push(RawToken {
                start_byte: span.0,
                end_byte: span.1,
                token_type: tok_type,
                modifiers: 0,
            });
        }
        prev
    }

    fn leave_call_statement(&mut self, prev_state: bool) {
        self.in_csr_stmt = prev_state;
    }

    fn on_register(&mut self, node: Node<'_>) {
        self.push_if_undeclared((node.start_byte(), node.end_byte()), IDX_REGISTER, 0);
    }

    fn on_numeric_label_ref(&mut self, node: Node<'_>, _raw: &str) {
        self.push_if_undeclared((node.start_byte(), node.end_byte()), IDX_LABEL, 0);
    }

    fn on_macro_var(&mut self, _param: &str, span: (usize, usize)) {
        self.push_if_undeclared(span, IDX_PARAMETER, 0);
    }

    fn on_macro_param_name(&mut self, node: Node<'_>) {
        self.push_if_undeclared(
            (node.start_byte(), node.end_byte()),
            IDX_PARAMETER,
            MOD_DECLARATION,
        );
    }

    fn on_macro_name(&mut self, node: Node<'_>) {
        self.push_if_undeclared((node.start_byte(), node.end_byte()), IDX_MACRO, 0);
    }

    fn on_symbol(&mut self, node: Node<'_>, name: &str) {
        let span = (node.start_byte(), node.end_byte());
        if !self.decl_spans.contains(&span)
            && let Some(tok_type) = self.classify_symbol_name(name)
        {
            self.out.push(RawToken {
                start_byte: span.0,
                end_byte: span.1,
                token_type: tok_type,
                modifiers: 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use lsp_types::Position;

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
        let uri = crate::context::test_uri("sem.S");
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
