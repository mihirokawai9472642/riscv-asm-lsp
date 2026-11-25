pub(crate) mod candidates;
pub(crate) mod snippets;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTag, CompletionList, CompletionResponse,
    CompletionTextEdit, InsertTextFormat, Position, Range, TextEdit,
};

use crate::context::{CompileContext, MarchSpec, effective_options_at_line};
use crate::data::Database;
use crate::document::Document;
use crate::index::{WorkspaceIndex, visible_params_at};

use candidates::{
    OPTION_SUBCOMMANDS, OperandQuery, ParenKind, ResolvePayload, collect_directive_completions,
    collect_instruction_completions, collect_operand_completions, push_macro_param_completions,
    push_option_arch_completions, push_relocation_completions, resolve_documentation,
    serialize_payload,
};

pub(super) const MAX_SYMBOL_CANDIDATES: usize = 200;

#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionClientCaps {
    pub snippet_support: bool,
    pub label_details_support: bool,
    pub resolve_documentation_support: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionContext {
    OptionSubcommand,
    OptionArchArg,
    SectionArg {
        arg_index: usize,
        in_quotes: bool,
    },
    Suppressed,
    StatementStart {
        prefix: String,
    },
    Operand {
        mnemonic: String,
        operand_index: usize,
        paren_kind: ParenKind,
    },
}

pub fn provide_completion(
    doc: &Document,
    pos: Position,
    db: &Database,
    ctx: &mut CompileContext,
    index: &mut WorkspaceIndex,
    caps: CompletionClientCaps,
) -> Option<CompletionResponse> {
    let file_ctx = ctx.for_uri(&doc.uri);
    let offset = doc.line_index.position_to_offset(&doc.text, pos);
    if crate::line::is_offset_in_comment(
        &doc.text,
        doc.tree.as_ref(),
        offset,
        file_ctx.preprocessed,
    ) {
        return None;
    }
    let (line_start, _, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
    let line_prefix = doc.text.get(line_start..offset)?;
    let starts_in_block =
        crate::line::in_block_comment_before(&doc.text, line_start, file_ctx.preprocessed);
    let (march, _) = effective_options_at_line(&doc.text, pos.line as usize, &file_ctx.march);
    let comp_ctx = classify_completion_context(line_prefix, starts_in_block, db, &march);
    if comp_ctx == CompletionContext::Suppressed {
        return None;
    }

    let include_sign = matches!(comp_ctx, CompletionContext::OptionArchArg);
    let (replace_start, typed_prefix) =
        completion_token_prefix(&doc.text, line_start, offset, include_sign);
    let replace_range = doc
        .line_index
        .byte_span_to_range(&doc.text, replace_start, offset);
    let typed_lower = typed_prefix.to_ascii_lowercase();

    let next_char = doc.text.get(offset..).and_then(|s| s.chars().next());
    let next_is_ws = next_char.is_some_and(|c| c == ' ' || c == '\t');
    let next_is_comma = next_char == Some(',');

    let visible = index.visible_symbols_for_completion(&doc.uri, offset, ctx);
    let in_macro_body = index
        .file_by_uri(&doc.uri)
        .is_some_and(|f| !visible_params_at(f, offset).is_empty());

    let mut items = Vec::new();
    let mut is_incomplete = false;

    match comp_ctx {
        CompletionContext::OptionSubcommand => {
            push_option_subcommands(&typed_lower, next_is_comma, replace_range, &mut items);
        }
        CompletionContext::OptionArchArg => {
            push_option_arch_completions(db, &typed_lower, replace_range, &mut items);
        }
        CompletionContext::SectionArg {
            arg_index,
            in_quotes,
        } => {
            candidates::push_section_arg_completions(
                arg_index,
                in_quotes,
                &typed_lower,
                replace_range,
                &mut items,
            );
        }
        CompletionContext::Suppressed => return None,
        CompletionContext::StatementStart { prefix } => {
            if prefix.starts_with('\\') && in_macro_body {
                push_macro_param_completions(
                    &visible.symbols,
                    &typed_lower,
                    replace_range,
                    "",
                    &mut items,
                );
            } else {
                collect_statement_start_completions(
                    StmtStartQuery {
                        db,
                        march: &march,
                        macros: visible.macros,
                        prefix: &prefix,
                        typed_lower: &typed_lower,
                        replace_range,
                        next_is_ws,
                        caps,
                    },
                    &mut items,
                    &mut is_incomplete,
                );
            }
        }
        CompletionContext::Operand {
            mnemonic,
            operand_index,
            paren_kind,
        } => {
            if typed_prefix.starts_with('%') {
                push_relocation_completions(db, &typed_lower, replace_range, &mut items);
            } else if typed_prefix.starts_with('\\') && in_macro_body {
                push_macro_param_completions(
                    &visible.symbols,
                    &typed_lower,
                    replace_range,
                    "",
                    &mut items,
                );
            } else {
                collect_operand_completions(
                    OperandQuery {
                        db,
                        march: &march,
                        mnemonic: &mnemonic,
                        operand_index,
                        paren_kind,
                        next_is_comma,
                        visible_syms: &visible.symbols,
                        in_macro_body,
                        typed_lower: &typed_lower,
                        replace_range,
                    },
                    &mut items,
                    &mut is_incomplete,
                );
            }
        }
    }

    if !caps.resolve_documentation_support {
        for item in &mut items {
            if item.documentation.is_none() {
                item.documentation = resolve_documentation(item, db);
            }
            item.data = None;
        }
    }

    Some(CompletionResponse::List(CompletionList {
        is_incomplete,
        items,
    }))
}

fn push_option_subcommands(
    typed_lower: &str,
    next_is_comma: bool,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    for &(name, _) in OPTION_SUBCOMMANDS {
        let rank = match_rank(name, typed_lower);
        let new_text = if name == "arch" && !next_is_comma {
            "arch, ".to_owned()
        } else {
            name.to_owned()
        };
        items.push(make_rich_item(RichItemSpec {
            label: name.to_owned(),
            filter_text: name.to_owned(),
            new_text,
            kind: CompletionItemKind::KEYWORD,
            detail: Some(".option".to_owned()),
            tags: None,
            data: serialize_payload(&ResolvePayload::Opt { n: name.to_owned() }),
            insert_text_format: None,
            sort_text: format!("{rank}_0_{name}"),
            replace_range,
        }));
    }
}

struct StmtStartQuery<'a> {
    db: &'a Database,
    march: &'a MarchSpec,
    macros: Vec<String>,
    prefix: &'a str,
    typed_lower: &'a str,
    replace_range: Range,
    next_is_ws: bool,
    caps: CompletionClientCaps,
}

fn collect_statement_start_completions(
    mut q: StmtStartQuery<'_>,
    items: &mut Vec<CompletionItem>,
    is_incomplete: &mut bool,
) {
    let wants_directive = q.prefix.starts_with('.');
    if !wants_directive {
        collect_instruction_completions(
            q.db,
            q.march,
            q.typed_lower,
            q.replace_range,
            q.next_is_ws,
            q.caps,
            items,
        );
        if q.macros.len() > MAX_SYMBOL_CANDIDATES {
            *is_incomplete = true;
            if !q.typed_lower.is_empty() {
                q.macros
                    .retain(|m| m.to_ascii_lowercase().contains(q.typed_lower));
            }
            q.macros
                .sort_by_key(|m| (match_rank(m, q.typed_lower), m.to_ascii_lowercase()));
            q.macros.truncate(MAX_SYMBOL_CANDIDATES);
        }
        for macro_name in q.macros {
            let rank = match_rank(&macro_name, q.typed_lower);
            let sort = format!("{rank}_1_{}", macro_name.to_ascii_lowercase());
            items.push(make_item(
                macro_name,
                CompletionItemKind::FUNCTION,
                Some("macro".to_owned()),
                sort,
                q.replace_range,
            ));
        }
    }
    collect_directive_completions(
        q.db,
        q.typed_lower,
        q.replace_range,
        q.next_is_ws,
        q.caps,
        items,
    );
}

pub fn resolve_completion_item(mut item: CompletionItem, db: &Database) -> CompletionItem {
    if item.documentation.is_none()
        && let Some(doc) = resolve_documentation(&item, db)
    {
        item.documentation = Some(doc);
    }
    item
}

fn completion_token_prefix(
    text: &str,
    line_start: usize,
    offset: usize,
    include_sign: bool,
) -> (usize, &str) {
    let bytes = text.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > line_start {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$') {
            start -= 1;
        } else {
            break;
        }
    }
    if start > line_start
        && (matches!(bytes[start - 1], b'%' | b'\\' | b'@')
            || (include_sign && matches!(bytes[start - 1], b'+' | b'-')))
    {
        start -= 1;
    }
    let slice = text.get(start..offset).unwrap_or("");
    (start, slice)
}

pub(super) fn match_rank(label: &str, typed_lower: &str) -> char {
    if typed_lower.is_empty() {
        return '1';
    }
    let lower = label.to_ascii_lowercase();
    if lower == typed_lower {
        '0'
    } else if lower.starts_with(typed_lower) {
        '1'
    } else {
        '2'
    }
}

pub(super) fn make_item(
    label: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    sort_text: String,
    replace_range: Range,
) -> CompletionItem {
    make_item_with_data(label, kind, detail, None, sort_text, replace_range)
}

pub(super) struct RichItemSpec {
    pub label: String,
    pub filter_text: String,
    pub new_text: String,
    pub kind: CompletionItemKind,
    pub detail: Option<String>,
    pub tags: Option<Vec<CompletionItemTag>>,
    pub data: Option<serde_json::Value>,
    pub insert_text_format: Option<InsertTextFormat>,
    pub sort_text: String,
    pub replace_range: Range,
}

pub(super) fn make_item_with_data(
    label: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    data: Option<serde_json::Value>,
    sort_text: String,
    replace_range: Range,
) -> CompletionItem {
    let filter_text = label.clone();
    let new_text = label.clone();
    make_rich_item(RichItemSpec {
        label,
        filter_text,
        new_text,
        kind,
        detail,
        tags: None,
        data,
        insert_text_format: None,
        sort_text,
        replace_range,
    })
}

pub(super) fn make_rich_item(s: RichItemSpec) -> CompletionItem {
    CompletionItem {
        filter_text: Some(s.filter_text),
        insert_text_format: s.insert_text_format,
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range: s.replace_range,
            new_text: s.new_text,
        })),
        label: s.label,
        kind: Some(s.kind),
        detail: s.detail,
        tags: s.tags,
        data: s.data,
        sort_text: Some(s.sort_text),
        ..CompletionItem::default()
    }
}

fn classify_completion_context(
    line_prefix: &str,
    starts_in_block: bool,
    db: &Database,
    march: &MarchSpec,
) -> CompletionContext {
    let active =
        crate::line::active_statement_prefix_with_block_state(line_prefix, starts_in_block);
    let after_label = active.as_str();

    if after_label.is_empty() {
        return CompletionContext::StatementStart {
            prefix: String::new(),
        };
    }

    let Some(ws_pos) = after_label.find(char::is_whitespace) else {
        return CompletionContext::StatementStart {
            prefix: after_label.to_owned(),
        };
    };

    let head = after_label[..ws_pos].trim().to_ascii_lowercase();
    let args = after_label[ws_pos..].trim_start();

    if head == ".option" {
        if !args.contains(',') && !args.chars().any(char::is_whitespace) {
            return CompletionContext::OptionSubcommand;
        }
        if args
            .to_ascii_lowercase()
            .strip_prefix("arch")
            .is_some_and(|r| r.trim_start().starts_with(','))
        {
            return CompletionContext::OptionArchArg;
        }
        return CompletionContext::Suppressed;
    }

    if head == ".section" || head == ".pushsection" {
        let (arg_index, _) = crate::line::count_operand_position(args);
        let in_quotes = is_inside_double_quotes(args);
        return CompletionContext::SectionArg {
            arg_index,
            in_quotes,
        };
    }

    let (operand_index, inside_parens) = crate::line::count_operand_position(args);
    let paren_kind = if inside_parens {
        classify_paren_kind(args, &head, operand_index, db, march)
    } else {
        ParenKind::None
    };
    CompletionContext::Operand {
        mnemonic: head,
        operand_index,
        paren_kind,
    }
}

fn is_inside_double_quotes(s: &str) -> bool {
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            escaped = true;
        } else if ch == '"' {
            in_quotes = !in_quotes;
        }
    }
    in_quotes
}

fn classify_paren_kind(
    args: &str,
    mnemonic: &str,
    operand_index: usize,
    db: &Database,
    march: &MarchSpec,
) -> ParenKind {
    let stripped = crate::line::strip_comments(args);
    let mut paren_stack: Vec<usize> = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = stripped.char_indices();
    while let Some((idx, ch)) = chars.next() {
        if in_single {
            if ch == '\\' {
                chars.next();
            } else if ch == '\'' {
                in_single = false;
            }
        } else if in_double {
            if ch == '\\' {
                chars.next();
            } else if ch == '"' {
                in_double = false;
            }
        } else {
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                '(' => paren_stack.push(idx),
                ')' => {
                    paren_stack.pop();
                }
                _ => {}
            }
        }
    }
    if let Some(&open_idx) = paren_stack.last() {
        let before = stripped[..open_idx].trim_end();
        let bytes = before.as_bytes();
        let mut i = bytes.len();
        while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i -= 1;
        }
        if i > 0 && bytes[i - 1] == b'%' {
            return ParenKind::Relocation;
        }
    }
    if candidates::has_memory_operand_at(db, march, mnemonic, operand_index) {
        return ParenKind::MemoryBase;
    }
    ParenKind::Expression
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use lsp_types::Documentation;
    use std::collections::BTreeSet;
    use std::fmt::Write;

    fn ctx_and_index(march: &str) -> (CompileContext, WorkspaceIndex) {
        let cfg = Config {
            default_march: march.to_owned(),
            ..Config::default()
        };
        (
            CompileContext::new(None, cfg),
            WorkspaceIndex::new(None).expect("index"),
        )
    }

    fn items_vec(resp: CompletionResponse) -> Vec<CompletionItem> {
        match resp {
            CompletionResponse::Array(arr) => arr,
            CompletionResponse::List(list) => list.items,
        }
    }

    fn labels(resp: CompletionResponse) -> BTreeSet<String> {
        items_vec(resp).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn statement_start_filters_by_march_and_excludes_udb_only() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_test.S");
        let doc = store.open(uri, 1, "    \n".to_owned());

        let (mut ctx32, mut idx32) = ctx_and_index("rv32i");
        idx32.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();
        let rv32i = labels(
            provide_completion(doc, Position::new(0, 4), &db, &mut ctx32, &mut idx32, plain)
                .expect("completion"),
        );
        assert!(rv32i.contains("addi"));
        assert!(rv32i.contains("la"));
        assert!(!rv32i.contains("mul"));
        assert!(!rv32i.contains("vadd.vv"));
        assert!(!rv32i.contains("fadd.q")); // UDB-only never offered

        let (mut ctx64, mut idx64) = ctx_and_index("rv64gcv");
        idx64.upsert_open_document(doc);
        let rv64gcv = labels(
            provide_completion(doc, Position::new(0, 4), &db, &mut ctx64, &mut idx64, plain)
                .expect("completion"),
        );
        assert!(rv64gcv.contains("mul"));
        assert!(rv64gcv.contains("vadd.vv"));
        assert!(rv64gcv.contains(".globl"));
    }

    #[test]
    fn operand_completion_offers_csrs_registers_options_and_macro_params() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_test.S");
        let doc = store.open(
            uri,
            1,
            "    csrw \n    csrw stvec, \n    .option \n.macro M reg\n    mv a0, \\\n.endm\n"
                .to_owned(),
        );
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();

        let csr_items = labels(
            provide_completion(doc, Position::new(0, 9), &db, &mut ctx, &mut index, plain)
                .expect("csr completion"),
        );
        assert!(csr_items.contains("stvec"));
        assert!(!csr_items.contains("a0"));
        assert!(!csr_items.contains("hstatus")); // H not enabled

        let reg_items = labels(
            provide_completion(doc, Position::new(1, 16), &db, &mut ctx, &mut index, plain)
                .expect("reg completion"),
        );
        assert!(reg_items.contains("a0"));
        assert!(reg_items.contains("sp"));
        assert!(!reg_items.contains("stvec"));

        let opt_items = labels(
            provide_completion(doc, Position::new(2, 12), &db, &mut ctx, &mut index, plain)
                .expect("opt completion"),
        );
        assert!(opt_items.contains("arch"));
        assert!(opt_items.contains("rvc"));
        assert!(opt_items.contains("norvc"));

        let macro_items = labels(
            provide_completion(doc, Position::new(4, 12), &db, &mut ctx, &mut index, plain)
                .expect("macro param completion"),
        );
        assert!(macro_items.contains("\\reg"));
    }

    #[test]
    fn completion_provides_text_edits_and_percent_relocations_only() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_edit.S");
        let doc = store.open(
            uri,
            1,
            "my_sym:\n    fadd.\n    .op\n    lui a0, %\n.macro M reg\n    mv a0, \\\n.endm\n"
                .to_owned(),
        );
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();

        // `fadd.` -> without resolveSupport, docs are inlined directly and `data` is omitted (Item 4)
        let arr = items_vec(
            provide_completion(doc, Position::new(1, 9), &db, &mut ctx, &mut index, plain).unwrap(),
        );
        let fadd_s = arr.iter().find(|i| i.label == "fadd.s").expect("fadd.s");
        let Some(CompletionTextEdit::Edit(te)) = &fadd_s.text_edit else {
            panic!("missing textEdit");
        };
        assert_eq!(
            te.range,
            Range::new(Position::new(1, 4), Position::new(1, 9))
        );
        assert_eq!(te.new_text, "fadd.s ");
        assert!(fadd_s.documentation.is_some());
        assert!(fadd_s.data.is_none());

        // `lui a0, %` -> only relocations, replacing 12..13
        let relocs = items_vec(
            provide_completion(doc, Position::new(3, 13), &db, &mut ctx, &mut index, plain)
                .unwrap(),
        );
        assert!(relocs.iter().all(|i| i.label.starts_with('%')));
        assert!(!relocs.iter().any(|i| i.label == "my_sym"));
        let Some(CompletionTextEdit::Edit(rte)) = &relocs[0].text_edit else {
            panic!("missing textEdit");
        };
        assert_eq!(
            rte.range,
            Range::new(Position::new(3, 12), Position::new(3, 13))
        );
    }

    #[test]
    fn snippet_variants_sort_real_before_alias_before_special_and_differentiate_labels() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_snip.S");
        let doc = store.open(uri, 1, "    \n    .\n".to_owned());
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);

        let caps = CompletionClientCaps {
            snippet_support: true,
            label_details_support: true,
            resolve_documentation_support: true,
        };
        let arr = items_vec(
            provide_completion(doc, Position::new(0, 4), &db, &mut ctx, &mut index, caps).unwrap(),
        );

        // `add` (Item 2): real instruction -> alias -> special TLS form
        let mut add_items: Vec<_> = arr
            .iter()
            .filter(|i| i.filter_text.as_deref() == Some("add"))
            .collect();
        add_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        let add_labels: Vec<&str> = add_items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            add_labels,
            vec![
                "add rd, rs1, rs2",
                "add rd, rs1, imm12",
                "add rd, rs1, rs2, src",
            ]
        );

        let addi = arr
            .iter()
            .find(|i| i.filter_text.as_deref() == Some("addi"))
            .expect("addi");
        assert_eq!(addi.label, "addi rd, rs1, imm12");
        assert_eq!(addi.detail.as_deref(), Some("Add immediate"));
        assert_eq!(addi.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let Some(CompletionTextEdit::Edit(te)) = &addi.text_edit else {
            panic!();
        };
        assert_eq!(te.new_text, "addi ${1:rd}, ${2:rs1}, ${3:imm12}$0");

        // `ld` (Item 4c): distinct labels & documentation start with variant syntax
        let mut ld_items: Vec<_> = arr
            .iter()
            .filter(|i| i.filter_text.as_deref() == Some("ld"))
            .collect();
        ld_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        assert!(ld_items.len() >= 2);
        assert_eq!(ld_items[0].label, "ld rd, imm12(rs1)");
        assert_eq!(ld_items[0].detail.as_deref(), Some("Load doubleword"));
        let Some(CompletionTextEdit::Edit(ld_first)) = &ld_items[0].text_edit else {
            panic!();
        };
        assert_eq!(ld_first.new_text, "ld ${1:rd}, ${2:0}(${3:rs1})$0");
        let ld_resolved = resolve_completion_item(ld_items[0].clone(), &db);
        let Some(Documentation::MarkupContent(ld_doc0)) = &ld_resolved.documentation else {
            panic!("expected markup doc on resolve");
        };
        assert!(ld_doc0.value.starts_with(
            "Instruction · Load doubleword · `RV64I` · RV64\n\n```riscv\nld rd, imm12(rs1)\n```"
        ));

        // Deprecated alias carries DEPRECATED tag and sorts after preferred instruction
        let mut dep_raw = db.raw.clone();
        dep_raw.instructions.push(crate::data::InstructionEntry {
            mnemonic: "legacy.alias".to_owned(),
            kind: crate::data::InstructionKind::Alias,
            variants: vec![crate::data::InstructionVariant {
                syntax: "legacy.alias rd".to_owned(),
                operands: vec![crate::data::OperandSpec {
                    name: "rd".to_owned(),
                    class: "GPR".to_owned(),
                }],
                required_extensions: vec!["i".to_owned()],
                xlen: crate::data::Xlen::Both,
                alias_of: Some("addi".to_owned()),
                encoding: None,
                alias_expansion: None,
            }],
            udb_assembly: None,
            isa_label: Some("I".to_owned()),
            long_name: None,
            summary: Some("Deprecated test alias.".to_owned()),
            description: None,
            description_source: None,
            llvm_supported: true,
            deprecated: true,
            preferred_name: Some("frcsr".to_owned()),
        });
        let dep_db = Database::from_data(dep_raw);
        let dep_arr = items_vec(
            provide_completion(
                doc,
                Position::new(0, 4),
                &dep_db,
                &mut ctx,
                &mut index,
                caps,
            )
            .unwrap(),
        );
        let deprecated_item = dep_arr
            .iter()
            .find(|i| i.filter_text.as_deref() == Some("legacy.alias"))
            .expect("legacy.alias");
        assert_eq!(
            deprecated_item.tags.as_deref(),
            Some([CompletionItemTag::DEPRECATED].as_slice())
        );
        let active_frcsr = dep_arr
            .iter()
            .find(|i| i.filter_text.as_deref() == Some("frcsr"))
            .expect("frcsr");
        assert!(active_frcsr.sort_text < deprecated_item.sort_text);

        // `csrw`: register variant must sort before immediate variant
        let mut csrw_items: Vec<_> = arr
            .iter()
            .filter(|i| i.filter_text.as_deref() == Some("csrw"))
            .collect();
        csrw_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        assert_eq!(csrw_items.len(), 2);
        assert_eq!(csrw_items[0].label, "csrw csr, rs");
        assert_eq!(csrw_items[1].label, "csrw csr, imm");
        let Some(CompletionTextEdit::Edit(csrw_reg)) = &csrw_items[0].text_edit else {
            panic!();
        };
        assert_eq!(csrw_reg.new_text, "csrw ${1:csr}, ${2:rs}$0");

        let ret = arr.iter().find(|i| i.label == "ret").expect("ret");
        assert_eq!(ret.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
        let Some(CompletionTextEdit::Edit(ret_te)) = &ret.text_edit else {
            panic!();
        };
        assert_eq!(ret_te.new_text, "ret");

        // Directive snippets
        let dirs = items_vec(
            provide_completion(doc, Position::new(1, 5), &db, &mut ctx, &mut index, caps).unwrap(),
        );
        let mac = dirs
            .iter()
            .find(|i| i.filter_text.as_deref() == Some(".macro"))
            .expect(".macro");
        assert_eq!(mac.label, ".macro name args");
        let Some(CompletionTextEdit::Edit(mac_te)) = &mac.text_edit else {
            panic!();
        };
        assert_eq!(mac_te.new_text, ".macro ${1:name} ${2:args}\n\t$0\n.endm");
        assert_eq!(mac.insert_text_format, Some(InsertTextFormat::SNIPPET));

        let mut opt_items: Vec<_> = dirs
            .iter()
            .filter(|i| i.filter_text.as_deref() == Some(".option"))
            .collect();
        opt_items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
        assert_eq!(opt_items.len(), 2);
        assert_eq!(opt_items[0].label, ".option push ... pop");
        let Some(CompletionTextEdit::Edit(opt_block)) = &opt_items[0].text_edit else {
            panic!();
        };
        assert_eq!(
            opt_block.new_text,
            ".option push\n.option ${1:norvc}\n$0\n.option pop"
        );
    }

    #[test]
    fn trailing_separators_outside_snippets() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_sep.S");
        let doc = store.open(
            uri,
            1,
            "my_lbl:\n    addi\n    ret\n    .globl\n    .text\n    .option \n    addi a\n    addi a0, a1, m\n    jal t\n"
                .to_owned(),
        );
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();

        let new_text_for = |arr: &[CompletionItem], label: &str| -> String {
            let item = arr.iter().find(|i| i.label == label).expect(label);
            let Some(CompletionTextEdit::Edit(te)) = &item.text_edit else {
                panic!("missing textEdit for {label}");
            };
            te.new_text.clone()
        };

        let l1 = items_vec(
            provide_completion(doc, Position::new(1, 8), &db, &mut ctx, &mut index, plain).unwrap(),
        );
        assert_eq!(new_text_for(&l1, "addi"), "addi ");
        assert_eq!(new_text_for(&l1, "ret"), "ret");
        assert_eq!(new_text_for(&l1, ".globl"), ".globl ");
        assert_eq!(new_text_for(&l1, ".text"), ".text");

        let l5 = items_vec(
            provide_completion(doc, Position::new(5, 12), &db, &mut ctx, &mut index, plain)
                .unwrap(),
        );
        assert_eq!(new_text_for(&l5, "arch"), "arch, ");
        assert_eq!(new_text_for(&l5, "rvc"), "rvc");

        let l6 = items_vec(
            provide_completion(doc, Position::new(6, 10), &db, &mut ctx, &mut index, plain)
                .unwrap(),
        );
        assert_eq!(new_text_for(&l6, "a0"), "a0, ");

        let l7 = items_vec(
            provide_completion(doc, Position::new(7, 16), &db, &mut ctx, &mut index, plain)
                .unwrap(),
        );
        assert_eq!(new_text_for(&l7, "my_lbl"), "my_lbl");

        let l8 = items_vec(
            provide_completion(doc, Position::new(8, 9), &db, &mut ctx, &mut index, plain).unwrap(),
        );
        assert_eq!(new_text_for(&l8, "t0"), "t0, ");
        assert_eq!(new_text_for(&l8, "my_lbl"), "my_lbl");
    }

    #[test]
    fn symbol_cap_sets_is_incomplete_and_filters_by_prefix() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_many_syms.S");
        let mut src = String::new();
        for i in 0..260 {
            let _ = writeln!(src, "sym_{i:03}:\n    nop");
        }
        src.push_str("    j \n    j sym_25\n");
        let doc = store.open(uri, 1, src);
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();

        // Line 520: `    j ` -> 260 symbols > MAX_SYMBOL_CANDIDATES (200) -> isIncomplete: true
        let CompletionResponse::List(unfiltered) =
            provide_completion(doc, Position::new(520, 6), &db, &mut ctx, &mut index, plain)
                .unwrap()
        else {
            panic!("expected CompletionList");
        };
        assert!(unfiltered.is_incomplete);
        let ref_count = unfiltered
            .items
            .iter()
            .filter(|i| i.kind == Some(CompletionItemKind::REFERENCE))
            .count();
        assert_eq!(ref_count, MAX_SYMBOL_CANDIDATES);

        // Line 521: `    j sym_25` -> prefix filter catches late symbol `sym_259`
        let CompletionResponse::List(filtered) = provide_completion(
            doc,
            Position::new(521, 12),
            &db,
            &mut ctx,
            &mut index,
            plain,
        )
        .unwrap() else {
            panic!("expected CompletionList");
        };
        assert!(filtered.items.iter().any(|i| i.label == "sym_259"));
    }

    #[test]
    fn no_completion_inside_block_and_line_comments_but_completes_after_closed_block() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = crate::context::test_uri("comp_comments.S");
        let src = "\
/* single line ad */
    /* indented ad */
/* x */ nop
/* a */ /* b ad */
/*
 ad
*/
addi a0, a1, 1 /* ad */
// ad
# ad
/* x */ ad
";
        let doc = store.open(uri, 1, src.to_owned());
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);
        let plain = CompletionClientCaps::default();

        // Suppressed inside every comment form
        for (line, col, desc) in [
            (0, 17, "single-line block col 0"),
            (1, 18, "indented single-line block"),
            (3, 15, "second block after closed block"),
            (5, 3, "multi-line block body"),
            (7, 20, "trailing block comment"),
            (8, 5, "double-slash line comment"),
            (9, 4, "hash line comment"),
        ] {
            let res = provide_completion(
                doc,
                Position::new(line, col),
                &db,
                &mut ctx,
                &mut index,
                plain,
            );
            assert!(res.is_none(), "expected no completion in {desc}");
        }

        // After a closed block comment on the same line, completion works normally
        let after_closed =
            provide_completion(doc, Position::new(2, 11), &db, &mut ctx, &mut index, plain)
                .expect("completion after /* x */ nop");
        let CompletionResponse::List(list) = after_closed else {
            panic!("expected CompletionList");
        };
        assert!(list.items.iter().any(|i| i.label == "nop"));

        let prefix_after_closed =
            provide_completion(doc, Position::new(10, 10), &db, &mut ctx, &mut index, plain)
                .expect("completion after /* x */ ad");
        let CompletionResponse::List(list2) = prefix_after_closed else {
            panic!("expected CompletionList");
        };
        assert!(list2.items.iter().any(|i| i.label == "addi"));
    }
}
