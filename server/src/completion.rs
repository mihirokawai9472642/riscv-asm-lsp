use std::collections::BTreeSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionResponse,
    CompletionTextEdit, Documentation, InsertTextFormat, MarkupContent, MarkupKind, Position,
    Range, TextEdit,
};

use crate::context::{CompileContext, MarchSpec, effective_options_at_line};
use crate::data::{Database, InstructionKind, InstructionVariant, RegisterClass};
use crate::document::Document;
use crate::index::{CompletionSymbol, SymKind, WorkspaceIndex, enclosing_macro_at};

const OPTION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("arch", "Modify or set active ISA architecture extensions"),
    ("rvc", "Enable compressed (C) instruction generation"),
    ("norvc", "Disable compressed (C) instruction generation"),
    ("push", "Push current .option state onto the option stack"),
    ("pop", "Restore previous .option state from the stack"),
    ("relax", "Enable linker relaxation"),
    ("norelax", "Disable linker relaxation"),
    ("pic", "Generate position-independent code"),
    ("nopic", "Generate position-dependent code"),
    (
        "exact",
        "Assemble instructions exactly as written without relaxation",
    ),
];

const RELOCATION_FUNCTIONS: &[(&str, &str)] = &[
    ("%hi(", "Bits [31:12] of absolute symbol address"),
    ("%lo(", "Bits [11:0] of absolute symbol address"),
    ("%pcrel_hi(", "Bits [31:12] of PC-relative symbol address"),
    (
        "%pcrel_lo(",
        "Bits [11:0] offset referencing the matching %pcrel_hi label",
    ),
    ("%tprel_hi(", "Bits [31:12] of TLS local-exec offset"),
    ("%tprel_lo(", "Bits [11:0] of TLS local-exec offset"),
    ("%tprel_add(", "TLS tp-relative add relocation annotation"),
    (
        "%got_pcrel_hi(",
        "Bits [31:12] of PC-relative GOT entry address",
    ),
];

const ROUNDING_MODES: &[(&str, &str)] = &[
    ("rne", "Round to Nearest, ties to Even"),
    ("rtz", "Round towards Zero"),
    ("rdn", "Round Down (towards -infinity)"),
    ("rup", "Round Up (towards +infinity)"),
    ("rmm", "Round to Nearest, ties to Max Magnitude"),
    ("dyn", "Dynamic rounding mode (use frm CSR)"),
];

const FENCE_ARGS: &[(&str, &str)] = &[
    ("iorw", "Input, Output, Read, Write"),
    ("rw", "Read and Write"),
    ("r", "Read"),
    ("w", "Write"),
    ("i", "Input"),
    ("o", "Output"),
];

#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionClientCaps {
    pub snippet_support: bool,
    pub label_details_support: bool,
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
        .and_then(|f| enclosing_macro_at(f, offset))
        .is_some();

    let mut items = Vec::new();

    match comp_ctx {
        CompletionContext::Comment => return None,
        CompletionContext::OptionSubcommand => {
            for (name, desc) in OPTION_SUBCOMMANDS {
                let rank = match_rank(name, &typed_lower);
                let new_text = if *name == "arch" && !next_is_comma {
                    "arch, ".to_owned()
                } else {
                    (*name).to_owned()
                };
                items.push(make_rich_item(
                    (*name).to_owned(),
                    new_text,
                    CompletionItemKind::KEYWORD,
                    Some(".option".to_owned()),
                    None,
                    Some(Documentation::String((*desc).to_owned())),
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
                    for macro_name in visible.macros {
                        let rank = match_rank(&macro_name, &typed_lower);
                        let sort = format!("{rank}_1_{}", macro_name.to_ascii_lowercase());
                        items.push(make_item(
                            macro_name,
                            CompletionItemKind::FUNCTION,
                            Some("macro".to_owned()),
                            None,
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
                );
            }
        }
    }

    Some(CompletionResponse::Array(items))
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

fn match_rank(label: &str, typed_lower: &str) -> char {
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

fn make_item(
    label: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    documentation: Option<Documentation>,
    sort_text: String,
    replace_range: Range,
) -> CompletionItem {
    let new_text = label.clone();
    make_rich_item(
        label,
        new_text,
        kind,
        detail,
        None,
        documentation,
        None,
        sort_text,
        replace_range,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_rich_item(
    label: String,
    new_text: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    label_details: Option<CompletionItemLabelDetails>,
    documentation: Option<Documentation>,
    insert_text_format: Option<InsertTextFormat>,
    sort_text: String,
    replace_range: Range,
) -> CompletionItem {
    CompletionItem {
        filter_text: Some(label.clone()),
        label_details,
        insert_text_format,
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range: replace_range,
            new_text,
        })),
        label,
        kind: Some(kind),
        detail,
        documentation,
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

fn collect_instruction_completions(
    db: &Database,
    march: &MarchSpec,
    typed_lower: &str,
    replace_range: Range,
    next_is_ws: bool,
    caps: CompletionClientCaps,
    items: &mut Vec<CompletionItem>,
) {
    for inst in &db.raw.instructions {
        if !inst.llvm_supported {
            continue;
        }
        let matching_variants: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
            .collect();
        if matching_variants.is_empty() {
            continue;
        }

        let kind = match inst.kind {
            InstructionKind::Instruction => CompletionItemKind::FUNCTION,
            InstructionKind::Alias | InstructionKind::Pseudo => CompletionItemKind::SNIPPET,
        };
        let documentation =
            format_instruction_doc(inst.long_name.as_deref(), inst.description.as_deref());
        let rank = match_rank(&inst.mnemonic, typed_lower);

        let mut unique_variants = deduplicate_and_sort_variants(&inst.mnemonic, &matching_variants);

        if !caps.snippet_support {
            let best = unique_variants.first().copied();
            let has_ops = unique_variants
                .iter()
                .any(|v| variant_slot_count(&inst.mnemonic, &v.syntax) > 0);
            let new_text = if has_ops && !next_is_ws {
                format!("{} ", inst.mnemonic)
            } else {
                inst.mnemonic.clone()
            };
            let detail = best.map(|v| v.syntax.clone());
            let sort = format!("{rank}_1_{}", inst.mnemonic);
            items.push(make_rich_item(
                inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                None,
                documentation,
                Some(InsertTextFormat::PLAIN_TEXT),
                sort,
                replace_range,
            ));
            continue;
        }

        for (idx, v) in unique_variants.drain(..).enumerate() {
            let args_str = syntax_args(&inst.mnemonic, &v.syntax);
            let (new_text, format) = build_instruction_snippet(&inst.mnemonic, args_str, v);
            let (detail, label_details) = if caps.label_details_support {
                let ld = if args_str.is_empty() {
                    None
                } else {
                    Some(CompletionItemLabelDetails {
                        detail: Some(format!(" {args_str}")),
                        description: None,
                    })
                };
                (inst.long_name.clone(), ld)
            } else {
                (Some(v.syntax.clone()), None)
            };
            let sort = format!("{rank}_1_{}_{idx:02}", inst.mnemonic);

            items.push(make_rich_item(
                inst.mnemonic.clone(),
                new_text,
                kind,
                detail,
                label_details,
                documentation.clone(),
                Some(format),
                sort,
                replace_range,
            ));
        }
    }
}

fn syntax_args<'a>(mnemonic: &str, syntax: &'a str) -> &'a str {
    let trimmed = syntax.trim();
    if let Some(rest) = trimmed.strip_prefix(mnemonic) {
        let next = rest.chars().next();
        if next.is_none_or(char::is_whitespace) {
            return rest.trim();
        }
    }
    trimmed
        .split_once(char::is_whitespace)
        .map_or("", |(_, r)| r.trim())
}

fn deduplicate_and_sort_variants<'a>(
    mnemonic: &str,
    variants: &[&'a InstructionVariant],
) -> Vec<&'a InstructionVariant> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &v in variants {
        let norm = v.syntax.split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.insert(norm) {
            out.push(v);
        }
    }
    out.sort_by_key(|v| variant_sort_key(mnemonic, v));
    out
}

fn variant_sort_key(mnemonic: &str, v: &InstructionVariant) -> (u8, u8, u8, usize, String) {
    let args = syntax_args(mnemonic, &v.syntax);
    // Prioritize: (1) register-taking / zero-arg forms before immediate-only forms,
    // (2) concrete register/offset forms before symbol pseudos,
    // (3) explicit offset memory forms `imm12(rs1)` before shorthand `(rs1)`.
    let has_sym = v.operands.iter().any(|o| o.class.contains("symbol"));
    let has_reg = v.operands.iter().any(|o| is_register_class(&o.class));
    let reg_score = u8::from(!args.is_empty() && !has_reg);
    let sym_score = u8::from(has_sym);
    let shorthand_mem_score = u8::from(
        split_top_level_slots(args)
            .iter()
            .any(|slot| split_mem_slot(slot).is_some_and(|(before, _)| before.is_empty())),
    );
    (
        reg_score,
        sym_score,
        shorthand_mem_score,
        v.operands.len(),
        v.syntax.clone(),
    )
}

fn is_register_class(class: &str) -> bool {
    class == "SP"
        || class.starts_with("GPR")
        || class == "SR07"
        || class.starts_with("AnyReg")
        || class.starts_with("FPR")
        || class.starts_with("VR")
}

fn build_instruction_snippet(
    mnemonic: &str,
    args_str: &str,
    v: &InstructionVariant,
) -> (String, InsertTextFormat) {
    if args_str.is_empty() {
        return (mnemonic.to_owned(), InsertTextFormat::PLAIN_TEXT);
    }

    let slots = split_top_level_slots(args_str);
    let mut tab_idx = 1usize;
    let mut formatted_slots = Vec::with_capacity(slots.len());

    for slot in slots {
        if let Some((before_paren, inside_paren)) = split_mem_slot(slot) {
            if before_paren.is_empty() {
                formatted_slots.push(format!("(${{{tab_idx}:{inside_paren}}})"));
                tab_idx += 1;
            } else {
                let first = tab_idx;
                let second = tab_idx + 1;
                tab_idx += 2;
                formatted_slots.push(format!("${{{first}:0}}(${{{second}:{inside_paren}}})"));
            }
        } else if v.operands.iter().any(|o| o.name == slot) {
            formatted_slots.push(format!("${{{tab_idx}:{slot}}}"));
            tab_idx += 1;
        } else {
            // Fixed literal operand such as `x0` in `c.addi x0, imm`
            formatted_slots.push(slot.to_owned());
        }
    }

    if tab_idx == 1 {
        return (
            format!("{mnemonic} {args_str}"),
            InsertTextFormat::PLAIN_TEXT,
        );
    }
    (
        format!("{mnemonic} {}$0", formatted_slots.join(", ")),
        InsertTextFormat::SNIPPET,
    )
}

fn split_mem_slot(slot: &str) -> Option<(&str, &str)> {
    let open = slot.find('(')?;
    let close = slot[open + 1..].find(')')?;
    let inside = slot[open + 1..open + 1 + close].trim();
    if inside.is_empty() {
        return None;
    }
    Some((slot[..open].trim(), inside))
}

fn split_top_level_slots(args_str: &str) -> Vec<&str> {
    if args_str.trim().is_empty() {
        return Vec::new();
    }
    let mut slots = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in args_str.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let part = args_str[start..idx].trim();
                if !part.is_empty() {
                    slots.push(part);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = args_str[start..].trim();
    if !tail.is_empty() {
        slots.push(tail);
    }
    slots
}

fn variant_slot_count(mnemonic: &str, syntax: &str) -> usize {
    split_top_level_slots(syntax_args(mnemonic, syntax)).len()
}

fn format_instruction_doc(long_name: Option<&str>, desc: Option<&str>) -> Option<Documentation> {
    let first = desc.map(first_sentence).filter(|s| !s.is_empty());
    let text = match (long_name, first) {
        (Some(ln), Some(d)) => format!("**{ln}**\n\n{d}"),
        (Some(ln), None) => format!("**{ln}**"),
        (None, Some(d)) => d,
        (None, None) => return None,
    };
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: text,
    }))
}

fn first_sentence(desc: &str) -> String {
    let para = desc.split("\n\n").next().unwrap_or(desc).trim();
    let mut in_tick = false;
    for (idx, ch) in para.char_indices() {
        if ch == '`' {
            in_tick = !in_tick;
        } else if ch == '.' && !in_tick {
            let next_ok = para[idx + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
            if next_ok {
                return para[..=idx].replace('\n', " ");
            }
        }
    }
    para.replace('\n', " ")
}

fn collect_directive_completions(
    db: &Database,
    typed_lower: &str,
    replace_range: Range,
    next_is_ws: bool,
    caps: CompletionClientCaps,
    items: &mut Vec<CompletionItem>,
) {
    for dir in &db.raw.directives {
        if !dir.llvm_supported {
            continue;
        }
        let rank = match_rank(&dir.name, typed_lower);

        if caps.snippet_support && dir.name == ".option" {
            let block_ld = caps
                .label_details_support
                .then(|| CompletionItemLabelDetails {
                    detail: Some(" push ... pop".to_owned()),
                    description: None,
                });
            items.push(make_rich_item(
                ".option".to_owned(),
                ".option push\n.option ${1:norvc}\n$0\n.option pop".to_owned(),
                CompletionItemKind::SNIPPET,
                if caps.label_details_support {
                    None
                } else {
                    dir.arguments.clone()
                },
                block_ld,
                Some(Documentation::String(dir.description.clone())),
                Some(InsertTextFormat::SNIPPET),
                format!("{rank}_2_.option_0"),
                replace_range,
            ));

            let inline_ld = caps
                .label_details_support
                .then(|| CompletionItemLabelDetails {
                    detail: Some(" <opt>".to_owned()),
                    description: None,
                });
            let inline_text = if next_is_ws {
                ".option".to_owned()
            } else {
                ".option ".to_owned()
            };
            items.push(make_rich_item(
                ".option".to_owned(),
                inline_text,
                CompletionItemKind::KEYWORD,
                if caps.label_details_support {
                    None
                } else {
                    dir.arguments.clone()
                },
                inline_ld,
                Some(Documentation::String(dir.description.clone())),
                Some(InsertTextFormat::PLAIN_TEXT),
                format!("{rank}_2_.option_1"),
                replace_range,
            ));
            continue;
        }

        let (new_text, format, detail_hint) = if caps.snippet_support
            && let Some((snippet, hint)) = directive_snippet(&dir.name)
        {
            (snippet.to_owned(), InsertTextFormat::SNIPPET, Some(hint))
        } else {
            let text = if !next_is_ws && dir.arguments.is_some() {
                format!("{} ", dir.name)
            } else {
                dir.name.clone()
            };
            (text, InsertTextFormat::PLAIN_TEXT, dir.arguments.as_deref())
        };

        let label_details = if caps.label_details_support {
            detail_hint.map(|h| CompletionItemLabelDetails {
                detail: Some(format!(" {h}")),
                description: None,
            })
        } else {
            None
        };
        let detail = if caps.label_details_support {
            None
        } else {
            dir.arguments.clone()
        };

        let sort = format!("{rank}_2_{}", dir.name);
        items.push(make_rich_item(
            dir.name.clone(),
            new_text,
            CompletionItemKind::KEYWORD,
            detail,
            label_details,
            Some(Documentation::String(dir.description.clone())),
            Some(format),
            sort,
            replace_range,
        ));
    }
}

fn directive_snippet(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        ".macro" => Some((".macro ${1:name} ${2:args}\n\t$0\n.endm", "name args")),
        ".rept" => Some((".rept ${1:count}\n\t$0\n.endr", "count")),
        ".irp" => Some((".irp ${1:sym}, ${2:values}\n\t$0\n.endr", "sym, values")),
        ".if" => Some((".if ${1:expr}\n\t$0\n.endif", "expr")),
        ".equ" => Some((".equ ${1:NAME}, ${2:value}$0", "NAME, value")),
        ".section" => Some((".section ${1:.text}$0", ".text")),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateHint {
    IntReg,
    FloatReg,
    VecReg,
    Csr,
    Frm,
    Fence,
    Vmask,
    SymbolOrImm,
}

#[allow(clippy::too_many_arguments)]
fn collect_operand_completions(
    db: &Database,
    march: &MarchSpec,
    mnemonic: &str,
    operand_index: usize,
    inside_parens: bool,
    next_is_comma: bool,
    visible_syms: &[CompletionSymbol],
    in_macro_body: bool,
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    let mut want_int = false;
    let mut want_float = false;
    let mut want_vec = false;
    let mut want_csr = false;
    let mut want_symbol_or_reloc = false;
    let mut want_frm = false;
    let mut want_fence = false;
    let mut want_vmask = false;
    let mut active_variants: Vec<&InstructionVariant> = Vec::new();

    if inside_parens {
        want_int = true;
        want_symbol_or_reloc = true;
    } else if let Some(inst) = db.instruction(mnemonic) {
        let variants: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| march.supports_extensions(&v.required_extensions, v.xlen))
            .collect();
        active_variants = if variants.is_empty() {
            inst.variants.iter().collect()
        } else {
            variants
        };

        let mut matched_any = false;
        for &v in &active_variants {
            if let Some(op) = v.operands.get(operand_index) {
                matched_any = true;
                classify_operand_class(
                    &op.class,
                    &mut want_int,
                    &mut want_float,
                    &mut want_vec,
                    &mut want_csr,
                    &mut want_symbol_or_reloc,
                    &mut want_frm,
                    &mut want_fence,
                    &mut want_vmask,
                );
            }
        }
        if !matched_any && operand_index == 0 {
            want_int = true;
        }
    } else {
        want_symbol_or_reloc = true;
        if !mnemonic.starts_with('.') {
            want_int = true;
        }
    }

    let sep_for = |hint: CandidateHint| {
        trailing_operand_sep(
            &active_variants,
            mnemonic,
            operand_index,
            inside_parens,
            next_is_comma,
            hint,
        )
    };

    if want_int {
        push_register_completions(
            db,
            RegisterClass::Integer,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::IntReg),
            items,
        );
    }
    if want_float {
        push_register_completions(
            db,
            RegisterClass::Float,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::FloatReg),
            items,
        );
    }
    if want_vec {
        push_register_completions(
            db,
            RegisterClass::Vector,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::VecReg),
            items,
        );
    }
    if want_csr {
        push_csr_completions(
            db,
            march,
            typed_lower,
            replace_range,
            sep_for(CandidateHint::Csr),
            items,
        );
    }
    if want_frm {
        let frm_sep = sep_for(CandidateHint::Frm);
        for (name, desc) in ROUNDING_MODES {
            let rank = match_rank(name, typed_lower);
            items.push(make_rich_item(
                (*name).to_owned(),
                format!("{name}{frm_sep}"),
                CompletionItemKind::ENUM_MEMBER,
                Some("rounding mode".to_owned()),
                None,
                Some(Documentation::String((*desc).to_owned())),
                None,
                format!("{rank}_0_{name}"),
                replace_range,
            ));
        }
    }
    if want_fence {
        let fence_sep = sep_for(CandidateHint::Fence);
        for (name, desc) in FENCE_ARGS {
            let rank = match_rank(name, typed_lower);
            items.push(make_rich_item(
                (*name).to_owned(),
                format!("{name}{fence_sep}"),
                CompletionItemKind::ENUM_MEMBER,
                Some("fence ordering".to_owned()),
                None,
                Some(Documentation::String((*desc).to_owned())),
                None,
                format!("{rank}_0_{name}"),
                replace_range,
            ));
        }
    }
    if want_vmask {
        let rank = match_rank("v0.t", typed_lower);
        let vmask_sep = sep_for(CandidateHint::Vmask);
        items.push(make_rich_item(
            "v0.t".to_owned(),
            format!("v0.t{vmask_sep}"),
            CompletionItemKind::VALUE,
            Some("vector mask register".to_owned()),
            None,
            None,
            None,
            format!("{rank}_0_v0.t"),
            replace_range,
        ));
    }
    if want_symbol_or_reloc {
        let sym_sep = sep_for(CandidateHint::SymbolOrImm);
        for sym in visible_syms {
            if sym.kind == SymKind::MacroParam {
                continue;
            }
            let kind = match sym.kind {
                SymKind::Equ | SymKind::Define => CompletionItemKind::CONSTANT,
                _ => CompletionItemKind::REFERENCE,
            };
            let rank = match_rank(&sym.name, typed_lower);
            let sort = format!("{rank}_2_{}", sym.name.to_ascii_lowercase());
            items.push(make_rich_item(
                sym.name.clone(),
                format!("{}{sym_sep}", sym.name),
                kind,
                sym.detail.clone(),
                None,
                None,
                None,
                sort,
                replace_range,
            ));
        }
        push_relocation_completions(typed_lower, replace_range, items);
    }
    if in_macro_body {
        let param_sep = if active_variants.is_empty() {
            ""
        } else {
            sep_for(CandidateHint::IntReg)
        };
        push_macro_param_completions(visible_syms, typed_lower, replace_range, param_sep, items);
    }
}

fn trailing_operand_sep(
    active_variants: &[&InstructionVariant],
    mnemonic: &str,
    operand_index: usize,
    inside_parens: bool,
    next_is_comma: bool,
    hint: CandidateHint,
) -> &'static str {
    if inside_parens || next_is_comma || active_variants.is_empty() {
        return "";
    }
    let sorted = deduplicate_and_sort_variants(mnemonic, active_variants);
    let matched = sorted
        .iter()
        .copied()
        .find(|v| {
            v.operands
                .get(operand_index)
                .is_some_and(|op| operand_class_matches_hint(&op.class, hint))
        })
        .or_else(|| sorted.first().copied());
    let Some(best) = matched else {
        return "";
    };
    let slots = variant_slot_count(mnemonic, &best.syntax);
    if operand_index + 1 < slots { ", " } else { "" }
}

fn operand_class_matches_hint(class: &str, hint: CandidateHint) -> bool {
    let mut int = false;
    let mut float = false;
    let mut vec = false;
    let mut csr = false;
    let mut sym = false;
    let mut frm = false;
    let mut fence = false;
    let mut vmask = false;
    classify_operand_class(
        class, &mut int, &mut float, &mut vec, &mut csr, &mut sym, &mut frm, &mut fence, &mut vmask,
    );
    match hint {
        CandidateHint::IntReg => int,
        CandidateHint::FloatReg => float,
        CandidateHint::VecReg => vec,
        CandidateHint::Csr => csr,
        CandidateHint::Frm => frm,
        CandidateHint::Fence => fence,
        CandidateHint::Vmask => vmask,
        CandidateHint::SymbolOrImm => sym,
    }
}

fn push_relocation_completions(
    typed_lower: &str,
    replace_range: Range,
    items: &mut Vec<CompletionItem>,
) {
    for (reloc, desc) in RELOCATION_FUNCTIONS {
        let rank = match_rank(reloc, typed_lower);
        items.push(make_item(
            (*reloc).to_owned(),
            CompletionItemKind::FUNCTION,
            Some("relocation modifier".to_owned()),
            Some(Documentation::String((*desc).to_owned())),
            format!("{rank}_3_{reloc}"),
            replace_range,
        ));
    }
}

fn push_macro_param_completions(
    visible_syms: &[CompletionSymbol],
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for sym in visible_syms {
        if sym.kind == SymKind::MacroParam {
            let rank = match_rank(&sym.name, typed_lower);
            items.push(make_rich_item(
                sym.name.clone(),
                format!("{}{sep}", sym.name),
                CompletionItemKind::VARIABLE,
                Some("macro parameter".to_owned()),
                None,
                None,
                None,
                format!("{rank}_0_{}", sym.name),
                replace_range,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_operand_class(
    class: &str,
    want_int: &mut bool,
    want_float: &mut bool,
    want_vec: &mut bool,
    want_csr: &mut bool,
    want_symbol_or_reloc: &mut bool,
    want_frm: &mut bool,
    want_fence: &mut bool,
    want_vmask: &mut bool,
) {
    if class == "csr_sysreg" {
        *want_csr = true;
    } else if class.starts_with("FPR") {
        if class.contains("INX") || class.contains("IN32X") {
            *want_int = true;
        } else {
            *want_float = true;
        }
    } else if class == "VMaskOp" {
        *want_vmask = true;
    } else if class.starts_with("VR") {
        *want_vec = true;
    } else if class.starts_with("GPR")
        || class.starts_with("SP")
        || class == "SR07"
        || class.starts_with("AnyReg")
    {
        *want_int = true;
    } else if class.starts_with("frmarg") {
        *want_frm = true;
    } else if class == "fencearg" {
        *want_fence = true;
    } else {
        *want_symbol_or_reloc = true;
    }
}

fn push_register_completions(
    db: &Database,
    class: RegisterClass,
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    let mut seen = BTreeSet::new();
    for reg in &db.raw.registers {
        if reg.class != class {
            continue;
        }
        // Prioritize ABI names (`_0_`) before architectural register names (`_1_`)
        for abi in &reg.abi_names {
            if seen.insert(abi.clone()) {
                let rank = match_rank(abi, typed_lower);
                items.push(make_rich_item(
                    abi.clone(),
                    format!("{abi}{sep}"),
                    CompletionItemKind::VARIABLE,
                    Some(reg.name.clone()),
                    None,
                    reg.description.clone().map(Documentation::String),
                    None,
                    format!("{rank}_0_{:02}_{abi}", reg.number),
                    replace_range,
                ));
            }
        }
        if seen.insert(reg.name.clone()) {
            let detail = if reg.abi_names.is_empty() {
                None
            } else {
                Some(reg.abi_names.join(", "))
            };
            let rank = match_rank(&reg.name, typed_lower);
            items.push(make_rich_item(
                reg.name.clone(),
                format!("{}{sep}", reg.name),
                CompletionItemKind::VARIABLE,
                detail,
                None,
                reg.description.clone().map(Documentation::String),
                None,
                format!("{rank}_1_{:02}_{}", reg.number, reg.name),
                replace_range,
            ));
        }
    }
}

fn push_csr_completions(
    db: &Database,
    march: &MarchSpec,
    typed_lower: &str,
    replace_range: Range,
    sep: &str,
    items: &mut Vec<CompletionItem>,
) {
    for csr in &db.raw.csrs {
        if !csr.llvm_supported || !march.supports_csr(&csr.required_extensions, csr.xlen) {
            continue;
        }
        let rank = match_rank(&csr.name, typed_lower);
        items.push(make_rich_item(
            csr.name.clone(),
            format!("{}{sep}", csr.name),
            CompletionItemKind::CONSTANT,
            Some(format!("0x{:03x}", csr.number)),
            None,
            csr.description.as_ref().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d.clone(),
                })
            }),
            None,
            format!("{rank}_0_{}", csr.name),
            replace_range,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::document::DocumentStore;
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

    fn labels(resp: CompletionResponse) -> BTreeSet<String> {
        let CompletionResponse::Array(arr) = resp else {
            return BTreeSet::new();
        };
        arr.into_iter().map(|i| i.label).collect()
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

        // `fadd.` -> replaces columns 4..9 and appends trailing space (E3)
        let CompletionResponse::Array(arr) =
            provide_completion(doc, Position::new(1, 9), &db, &mut ctx, &mut index, plain).unwrap()
        else {
            panic!();
        };
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

        // `lui a0, %` -> only relocations, replacing 12..13
        let CompletionResponse::Array(relocs) =
            provide_completion(doc, Position::new(3, 13), &db, &mut ctx, &mut index, plain)
                .unwrap()
        else {
            panic!();
        };
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
    fn snippet_and_label_details_completion() {
        let db = Database::load().expect("db");
        let mut store = DocumentStore::new().expect("store");
        let uri = lsp_types::Uri::from_str("file:///snip.S").expect("uri");
        let doc = store.open(uri, 1, "    \n    .\n".to_owned());
        let (mut ctx, mut index) = ctx_and_index("rv64gc");
        index.upsert_open_document(doc);

        let caps = CompletionClientCaps {
            snippet_support: true,
            label_details_support: true,
        };
        let CompletionResponse::Array(arr) =
            provide_completion(doc, Position::new(0, 4), &db, &mut ctx, &mut index, caps).unwrap()
        else {
            panic!();
        };

        let addi = arr.iter().find(|i| i.label == "addi").expect("addi");
        assert_eq!(addi.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(
            addi.label_details
                .as_ref()
                .and_then(|d| d.detail.as_deref()),
            Some(" rd, rs1, imm12")
        );
        let Some(CompletionTextEdit::Edit(te)) = &addi.text_edit else {
            panic!();
        };
        assert_eq!(te.new_text, "addi ${1:rd}, ${2:rs1}, ${3:imm12}$0");

        let ld_items: Vec<_> = arr.iter().filter(|i| i.label == "ld").collect();
        assert!(ld_items.len() >= 2);
        let Some(CompletionTextEdit::Edit(ld_first)) = &ld_items[0].text_edit else {
            panic!();
        };
        assert_eq!(ld_first.new_text, "ld ${1:rd}, ${2:0}(${3:rs1})$0");

        // `csrw`: register variant must sort before immediate variant
        let csrw_items: Vec<_> = arr.iter().filter(|i| i.label == "csrw").collect();
        assert_eq!(csrw_items.len(), 2);
        assert!(csrw_items[0].sort_text < csrw_items[1].sort_text);
        assert_eq!(
            csrw_items[0]
                .label_details
                .as_ref()
                .and_then(|d| d.detail.as_deref()),
            Some(" csr, rs")
        );
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
        let CompletionResponse::Array(dirs) =
            provide_completion(doc, Position::new(1, 5), &db, &mut ctx, &mut index, caps).unwrap()
        else {
            panic!();
        };
        let mac = dirs.iter().find(|i| i.label == ".macro").expect(".macro");
        let Some(CompletionTextEdit::Edit(mac_te)) = &mac.text_edit else {
            panic!();
        };
        assert_eq!(mac_te.new_text, ".macro ${1:name} ${2:args}\n\t$0\n.endm");
        assert_eq!(mac.insert_text_format, Some(InsertTextFormat::SNIPPET));

        let opt_items: Vec<_> = dirs.iter().filter(|i| i.label == ".option").collect();
        assert_eq!(opt_items.len(), 2);
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

        // Line 1 (`    addi`) -> `"addi "`
        let CompletionResponse::Array(l1) =
            provide_completion(doc, Position::new(1, 8), &db, &mut ctx, &mut index, plain).unwrap()
        else {
            panic!();
        };
        assert_eq!(new_text_for(&l1, "addi"), "addi ");
        assert_eq!(new_text_for(&l1, "ret"), "ret"); // Line 1 also offers `ret` (0 ops -> no space)
        assert_eq!(new_text_for(&l1, ".globl"), ".globl ");
        assert_eq!(new_text_for(&l1, ".text"), ".text");

        // Line 5 (`    .option `) -> `arch` gets `", "`, `rvc` gets nothing
        let CompletionResponse::Array(l5) =
            provide_completion(doc, Position::new(5, 12), &db, &mut ctx, &mut index, plain)
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(new_text_for(&l5, "arch"), "arch, ");
        assert_eq!(new_text_for(&l5, "rvc"), "rvc");

        // Line 6 (`    addi a`) -> operand 0 (`rd`) gets `", "`
        let CompletionResponse::Array(l6) =
            provide_completion(doc, Position::new(6, 10), &db, &mut ctx, &mut index, plain)
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(new_text_for(&l6, "a0"), "a0, ");

        // Line 7 (`    addi a0, a1, m`) -> last operand gets nothing
        let CompletionResponse::Array(l7) =
            provide_completion(doc, Position::new(7, 16), &db, &mut ctx, &mut index, plain)
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(new_text_for(&l7, "my_lbl"), "my_lbl");

        // Line 8 (`    jal t`) -> register `t0` matches `jal rd, imm20` (`, `), label `my_lbl` matches `jal offset` (``)
        let CompletionResponse::Array(l8) =
            provide_completion(doc, Position::new(8, 9), &db, &mut ctx, &mut index, plain).unwrap()
        else {
            panic!();
        };
        assert_eq!(new_text_for(&l8, "t0"), "t0, ");
        assert_eq!(new_text_for(&l8, "my_lbl"), "my_lbl");
    }
}
