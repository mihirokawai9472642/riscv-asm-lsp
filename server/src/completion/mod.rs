mod candidates;
mod snippets;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTag, CompletionList, CompletionResponse,
    CompletionTextEdit, InsertTextFormat, Position, Range, TextEdit,
};

use crate::context::{CompileContext, effective_options_at_line};
use crate::data::Database;
use crate::document::Document;
use crate::index::{WorkspaceIndex, visible_params_at};

use candidates::{
    OPTION_SUBCOMMANDS, ResolvePayload, collect_directive_completions,
    collect_instruction_completions, collect_operand_completions, push_macro_param_completions,
    push_relocation_completions, resolve_documentation, serialize_payload,
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
    Comment,
    OptionSubcommand,
    StatementStart {
        prefix: String,
    },
    Operand {
        mnemonic: String,
        operand_index: usize,
        inside_parens: bool,
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
    let (line_start, _, _) = doc.line_index.line_bounds(&doc.text, pos.line as usize);
    let line_prefix = doc.text.get(line_start..offset)?;

    let comp_ctx = classify_completion_context(line_prefix);
    if comp_ctx == CompletionContext::Comment {
        return None;
    }

    let (replace_start, typed_prefix) = completion_token_prefix(&doc.text, line_start, offset);
    let replace_range = doc
        .line_index
        .byte_span_to_range(&doc.text, replace_start, offset);
    let typed_lower = typed_prefix.to_ascii_lowercase();

    let next_char = doc.text.get(offset..).and_then(|s| s.chars().next());
    let next_is_ws = next_char.is_some_and(|c| c == ' ' || c == '\t');
    let next_is_comma = next_char == Some(',');

    let (march, _) = effective_options_at_line(&doc.text, pos.line as usize, &file_ctx.march);
    let visible = index.visible_symbols_for_completion(&doc.uri, offset, ctx);
    let in_macro_body = index
        .file_by_uri(&doc.uri)
        .is_some_and(|f| !visible_params_at(f, offset).is_empty());

    let mut items = Vec::new();
    let mut is_incomplete = false;

    match comp_ctx {
        CompletionContext::Comment => return None,
        CompletionContext::OptionSubcommand => {
            for &(name, _) in OPTION_SUBCOMMANDS {
                let rank = match_rank(name, &typed_lower);
                let new_text = if name == "arch" && !next_is_comma {
                    "arch, ".to_owned()
                } else {
                    name.to_owned()
                };
                items.push(make_rich_item(
                    name.to_owned(),
                    name.to_owned(),
                    new_text,
                    CompletionItemKind::KEYWORD,
                    Some(".option".to_owned()),
                    None,
                    serialize_payload(&ResolvePayload::Opt { n: name.to_owned() }),
                    None,
                    format!("{rank}_0_{name}"),
                    replace_range,
                ));
            }
        }
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
                let wants_directive = prefix.starts_with('.');
                if !wants_directive {
                    collect_instruction_completions(
                        db,
                        &march,
                        &typed_lower,
                        replace_range,
                        next_is_ws,
                        caps,
                        &mut items,
                    );
                    let mut macros = visible.macros;
                    if macros.len() > MAX_SYMBOL_CANDIDATES {
                        is_incomplete = true;
                        if !typed_lower.is_empty() {
                            macros.retain(|m| m.to_ascii_lowercase().contains(&typed_lower));
                        }
                        macros
                            .sort_by_key(|m| (match_rank(m, &typed_lower), m.to_ascii_lowercase()));
                        macros.truncate(MAX_SYMBOL_CANDIDATES);
                    }
                    for macro_name in macros {
                        let rank = match_rank(&macro_name, &typed_lower);
                        let sort = format!("{rank}_1_{}", macro_name.to_ascii_lowercase());
                        items.push(make_item(
                            macro_name,
                            CompletionItemKind::FUNCTION,
                            Some("macro".to_owned()),
                            sort,
                            replace_range,
                        ));
                    }
                }
                collect_directive_completions(
                    db,
                    &typed_lower,
                    replace_range,
                    next_is_ws,
                    caps,
                    &mut items,
                );
            }
        }
        CompletionContext::Operand {
            mnemonic,
            operand_index,
            inside_parens,
        } => {
            if typed_prefix.starts_with('%') {
                push_relocation_completions(&typed_lower, replace_range, &mut items);
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
                    db,
                    &march,
                    &mnemonic,
                    operand_index,
                    inside_parens,
                    next_is_comma,
                    &visible.symbols,
                    in_macro_body,
                    &typed_lower,
                    replace_range,
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

pub fn resolve_completion_item(mut item: CompletionItem, db: &Database) -> CompletionItem {
    if item.documentation.is_none()
        && let Some(doc) = resolve_documentation(&item, db)
    {
        item.documentation = Some(doc);
    }
    item
}

fn completion_token_prefix(text: &str, line_start: usize, offset: usize) -> (usize, &str) {
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
    if start > line_start && matches!(bytes[start - 1], b'%' | b'\\') {
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
    make_rich_item(
        label,
        filter_text,
        new_text,
        kind,
        detail,
        None,
        data,
        None,
        sort_text,
        replace_range,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn make_rich_item(
    label: String,
    filter_text: String,
    new_text: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    tags: Option<Vec<CompletionItemTag>>,
    data: Option<serde_json::Value>,
    insert_text_format: Option<InsertTextFormat>,
    sort_text: String,
    replace_range: Range,
) -> CompletionItem {
    CompletionItem {
        filter_text: Some(filter_text),
        insert_text_format,
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range: replace_range,
            new_text,
        })),
        label,
        kind: Some(kind),
        detail,
        tags,
        data,
        sort_text: Some(sort_text),
        ..CompletionItem::default()
    }
}

fn classify_completion_context(line_prefix: &str) -> CompletionContext {
    if has_unquoted_comment(line_prefix) {
        return CompletionContext::Comment;
    }

    let stmt = last_unquoted_statement(line_prefix);
    let after_label = strip_leading_label(stmt).trim_start();

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
        return CompletionContext::OptionSubcommand;
    }

    let (operand_index, inside_parens) = count_operand_position(args);
    CompletionContext::Operand {
        mnemonic: head,
        operand_index,
        inside_parens,
    }
}

fn has_unquoted_comment(s: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
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
                '#' => return true,
                '/' if chars.peek() == Some(&'/') => return true,
                _ => {}
            }
        }
    }
    false
}

fn last_unquoted_statement(s: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut last_semi = 0;
    let mut chars = s.char_indices().peekable();
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
                ';' => last_semi = idx + 1,
                _ => {}
            }
        }
    }
    &s[last_semi..]
}

fn strip_leading_label(stmt: &str) -> &str {
    let mut rest = stmt.trim_start();
    loop {
        let mut colon_pos = None;
        for (idx, ch) in rest.char_indices() {
            if ch == ':' {
                colon_pos = Some(idx);
                break;
            }
            if !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | '.' | '$') && !ch.is_whitespace()
            {
                break;
            }
        }
        let Some(pos) = colon_pos else {
            break;
        };
        let label_part = rest[..pos].trim();
        if label_part.is_empty() || label_part.contains(char::is_whitespace) {
            break;
        }
        rest = rest[pos + 1..].trim_start();
    }
    rest
}

fn count_operand_position(args: &str) -> (usize, bool) {
    let mut depth = 0usize;
    let mut commas = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = args.chars().peekable();

    while let Some(ch) = chars.next() {
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
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => commas += 1,
                _ => {}
            }
        }
    }
    (commas, depth > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
    use lsp_types::Documentation;
    use std::collections::BTreeSet;
    use std::fmt::Write;
    use std::str::FromStr;

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
        let uri = lsp_types::Uri::from_str("file:///test.S").expect("uri");
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
        let uri = lsp_types::Uri::from_str("file:///test.S").expect("uri");
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
        let uri = lsp_types::Uri::from_str("file:///edit.S").expect("uri");
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
        let uri = lsp_types::Uri::from_str("file:///snip.S").expect("uri");
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
        assert!(
            ld_doc0
                .value
                .starts_with("```riscv\nld rd, imm12(rs1)\n```")
        );

        // Deprecated alias carries DEPRECATED tag and sorts after preferred instruction
        let deprecated_vpopc = arr
            .iter()
            .find(|i| i.filter_text.as_deref() == Some("frsr"))
            .expect("frsr");
        assert_eq!(
            deprecated_vpopc.tags.as_deref(),
            Some([CompletionItemTag::DEPRECATED].as_slice())
        );
        let active_frcsr = arr
            .iter()
            .find(|i| i.filter_text.as_deref() == Some("frcsr"))
            .expect("frcsr");
        assert!(active_frcsr.sort_text < deprecated_vpopc.sort_text);

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
        let uri = lsp_types::Uri::from_str("file:///sep.S").expect("uri");
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
        let uri = lsp_types::Uri::from_str("file:///many_syms.S").expect("uri");
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
}
