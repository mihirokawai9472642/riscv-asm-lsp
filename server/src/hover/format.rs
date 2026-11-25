use std::collections::HashSet;
use std::fmt::Write;

use crate::context::{MarchSpec, uri_to_path};
use crate::data::{
    CsrEntry, Database, DirectiveEntry, InstructionEntry, InstructionKind, InstructionVariant,
    RegisterClass, RegisterEntry, Xlen,
};
use crate::index::{AssignKind, IncludeKind, SymKind, SymbolHoverInfo};

use super::operands::describe_operand_class;

const MAX_SYNTAX_VARIANTS: usize = 3;
const MAX_CSR_FIELDS: usize = 8;

#[derive(Debug, Clone)]
pub struct RenderedHoverParts {
    pub context: String,
    pub lang: &'static str,
    pub code: String,
    pub below_fence: Option<String>,
    pub notices: Vec<String>,
    pub facts: Vec<String>,
    pub prose: Option<String>,
}

impl RenderedHoverParts {
    pub fn render(&self) -> String {
        let mut md = String::new();
        let _ = write!(
            md,
            "{}\n\n```{}\n{}\n```",
            self.context, self.lang, self.code
        );
        if let Some(below) = &self.below_fence {
            let _ = write!(md, "\n{below}");
        }
        for notice in &self.notices {
            let _ = write!(md, "\n\n> {notice}");
        }
        if !self.facts.is_empty() {
            md.push_str("\n\n---\n\n");
            for (idx, fact) in self.facts.iter().enumerate() {
                if idx > 0 {
                    let prev = &self.facts[idx - 1];
                    if prev.contains('\n') || fact.contains('\n') {
                        md.push_str("\n\n");
                    } else {
                        md.push_str("  \n");
                    }
                }
                md.push_str(fact);
            }
        }
        if let Some(prose) = &self.prose {
            let trimmed = prose.trim();
            if !trimmed.is_empty() {
                let _ = write!(md, "\n\n---\n\n{trimmed}");
            }
        }
        md.trim_end().to_owned()
    }
}

pub(crate) fn build_instruction_hover_parts(
    inst: &InstructionEntry,
    effective_march: &MarchSpec,
    enabled_by_march: bool,
    db: &Database,
    specific_syntax: Option<&str>,
) -> RenderedHoverParts {
    let variants_to_show = select_variants(inst, effective_march, specific_syntax);
    let (code, below_fence) = collect_syntax_block(inst, &variants_to_show);
    let context = format_instruction_context(inst, &variants_to_show);

    let mut notices = Vec::new();
    if !inst.llvm_supported {
        notices.push("Not accepted by LLVM 19 (spec only).".to_owned());
    } else if !enabled_by_march {
        notices.push(format_march_disabled_notice(inst, effective_march));
    }
    if inst.deprecated {
        if let Some(pref) = &inst.preferred_name {
            notices.push(format!("Deprecated — prefer `{pref}`."));
        } else {
            notices.push("Deprecated.".to_owned());
        }
    }

    let mut facts = Vec::new();
    let aliases = collect_aliases(inst);
    if !aliases.is_empty() {
        let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
        facts.push(format!("Alias of: {}", targets.join(", ")));
    }
    if let Some(op_fact) = format_operands_fact(&variants_to_show, db) {
        facts.push(op_fact);
    }

    let prose = inst
        .summary
        .clone()
        .or_else(|| {
            inst.description
                .as_deref()
                .map(fallback_first_sentence)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            inst.llvm_supported
                .then(|| "*No description in RISC-V specification.*".to_owned())
        });

    RenderedHoverParts {
        context,
        lang: "riscv",
        code,
        below_fence,
        notices,
        facts,
        prose,
    }
}

pub(crate) fn format_instruction_completion_doc(
    inst: &InstructionEntry,
    effective_march: &MarchSpec,
    db: &Database,
    specific_syntax: Option<&str>,
) -> String {
    build_instruction_hover_parts(inst, effective_march, true, db, specific_syntax).render()
}

fn select_variants<'a>(
    inst: &'a InstructionEntry,
    effective_march: &MarchSpec,
    specific_syntax: Option<&str>,
) -> Vec<&'a InstructionVariant> {
    let ext_matching: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| effective_march.supports_extensions(&v.required_extensions, v.xlen))
        .collect();
    let pool: Vec<_> = if !ext_matching.is_empty() {
        ext_matching
    } else {
        let xlen_matching: Vec<_> = inst
            .variants
            .iter()
            .filter(|v| effective_march.supports_xlen(v.xlen))
            .collect();
        if xlen_matching.is_empty() {
            inst.variants.iter().collect()
        } else {
            xlen_matching
        }
    };
    let sorted = crate::completion::snippets::deduplicate_and_sort_variants(&inst.mnemonic, &pool);
    if let Some(target) = specific_syntax {
        let norm_target = normalize_syntax(target);
        let narrowed: Vec<_> = sorted
            .iter()
            .copied()
            .filter(|v| normalize_syntax(&v.syntax) == norm_target)
            .collect();
        if !narrowed.is_empty() {
            return narrowed;
        }
    }
    sorted
}

fn collect_syntax_block(
    inst: &InstructionEntry,
    variants: &[&InstructionVariant],
) -> (String, Option<String>) {
    let mut seen_syntax = HashSet::new();
    let mut unique_syntaxes = Vec::new();
    for v in variants {
        let norm = normalize_syntax(&v.syntax);
        if seen_syntax.insert(norm) {
            unique_syntaxes.push(v.syntax.as_str());
        }
    }
    if unique_syntaxes.is_empty() {
        return (inst.mnemonic.clone(), None);
    }
    let total = unique_syntaxes.len();
    let shown: Vec<&str> = unique_syntaxes
        .into_iter()
        .take(MAX_SYNTAX_VARIANTS)
        .collect();
    let below = (total > MAX_SYNTAX_VARIANTS)
        .then(|| format!("*+{} more variants*", total - MAX_SYNTAX_VARIANTS));
    (shown.join("\n"), below)
}

fn format_instruction_context(inst: &InstructionEntry, variants: &[&InstructionVariant]) -> String {
    let kind_label = match inst.kind {
        InstructionKind::Instruction => "Instruction",
        InstructionKind::Alias => "Alias",
        InstructionKind::Pseudo => "Pseudo-instruction",
    };

    let mut exts = Vec::new();
    let mut has_rv32 = false;
    let mut has_rv64 = false;
    for v in variants {
        for req in &v.required_extensions {
            let formatted = format_ext_req_inline(req);
            if !exts.contains(&formatted) {
                exts.push(formatted);
            }
        }
        match v.xlen {
            Xlen::Both => {
                has_rv32 = true;
                has_rv64 = true;
            }
            Xlen::Rv32 => has_rv32 = true,
            Xlen::Rv64 => has_rv64 = true,
        }
    }

    let xlen_label = match (has_rv32, has_rv64) {
        (true, false) => "RV32",
        (false, true) => "RV64",
        _ => "RV32/RV64",
    };

    let ext_part = if exts.is_empty() {
        if let Some(isa) = &inst.isa_label {
            format!("`{isa}`")
        } else {
            let base_badge = match (has_rv32, has_rv64) {
                (true, false) => "RV32I",
                (false, true) => "RV64I",
                _ => "I",
            };
            format!("`{base_badge}`")
        }
    } else {
        exts.iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut meta = vec![kind_label.to_owned()];
    if let Some(long_name) = &inst.long_name {
        meta.push(long_name.clone());
    }
    meta.push(ext_part);
    meta.push(xlen_label.to_owned());
    if !inst.llvm_supported {
        meta.push("spec only".to_owned());
    }
    meta.join(" · ")
}

fn collect_aliases(inst: &InstructionEntry) -> Vec<&str> {
    let mut seen = HashSet::new();
    let mut aliases = Vec::new();
    for v in &inst.variants {
        if let Some(alias) = v.alias_of.as_deref()
            && !alias.eq_ignore_ascii_case(&inst.mnemonic)
            && seen.insert(alias.to_ascii_lowercase())
        {
            aliases.push(alias);
        }
    }
    aliases
}

fn format_operands_fact(variants: &[&InstructionVariant], db: &Database) -> Option<String> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen_name: HashSet<String> = HashSet::new();

    for v in variants.iter().take(MAX_SYNTAX_VARIANTS) {
        for op in &v.operands {
            if !seen_name.insert(op.name.clone()) {
                continue;
            }
            let Some(desc) = describe_operand_class(&op.class, db) else {
                continue;
            };
            if let Some((_, names)) = groups.iter_mut().find(|(d, _)| *d == desc) {
                names.push(op.name.clone());
            } else {
                groups.push((desc, vec![op.name.clone()]));
            }
        }
    }

    if groups.is_empty() {
        return None;
    }

    let parts: Vec<String> = groups
        .into_iter()
        .take(5)
        .map(|(desc, names)| {
            let formatted_names = names
                .into_iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{formatted_names} {desc}")
        })
        .collect();
    Some(format!("Operands: {}", parts.join("; ")))
}

fn format_march_disabled_notice(inst: &InstructionEntry, effective_march: &MarchSpec) -> String {
    let xlen_ok: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| effective_march.supports_xlen(v.xlen))
        .collect();

    if xlen_ok.is_empty() {
        return format!(
            "Requires a different XLEN; not enabled by `-march={}`.",
            effective_march.raw
        );
    }

    let mut reqs = Vec::new();
    for v in xlen_ok {
        if v.required_extensions.is_empty() {
            continue;
        }
        let joined = v
            .required_extensions
            .iter()
            .map(|r| format_ext_req_notice(r))
            .collect::<Vec<_>>()
            .join(" + ");
        if !reqs.contains(&joined) {
            reqs.push(joined);
        }
    }
    if reqs.is_empty() {
        format!("Not enabled by `-march={}`.", effective_march.raw)
    } else {
        format!(
            "Requires {}; not enabled by `-march={}`.",
            reqs.join(" / "),
            effective_march.raw
        )
    }
}

fn format_ext_name(ext: &str) -> String {
    let lower = ext.trim().to_ascii_lowercase();
    if lower.len() == 1 {
        lower.to_ascii_uppercase()
    } else {
        let mut chars = lower.chars();
        match chars.next() {
            Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
            None => lower,
        }
    }
}

fn format_ext_req_inline(req: &str) -> String {
    let parts: Vec<String> = req.split('|').map(format_ext_name).collect();
    parts.join(" or ")
}

fn format_ext_req_notice(req: &str) -> String {
    let parts: Vec<String> = req.split('|').map(format_ext_name).collect();
    if parts.len() <= 1 {
        parts.into_iter().next().unwrap_or_default()
    } else {
        format!("{} (or {})", parts[0], parts[1..].join(", "))
    }
}

fn normalize_syntax(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn format_csr_hover(csr: &CsrEntry, effective_march: Option<&MarchSpec>) -> String {
    let mut header = vec!["CSR".to_owned()];
    if let Some(long_name) = csr
        .description
        .as_deref()
        .and_then(extract_leading_bold_title)
    {
        header.push(long_name.to_owned());
    }
    header.push(format!("0x{:03x}", csr.number));
    if let Some(priv_label) = csr.privilege_label() {
        header.push(priv_label.to_owned());
    }
    if !csr.llvm_supported {
        header.push("spec only".to_owned());
    }
    let context = header.join(" · ");

    let is_rv32 = effective_march.is_some_and(|m| m.xlen == Xlen::Rv32);
    let mut facts = Vec::new();
    if !csr.fields.is_empty() {
        let mut list = String::from("Fields:\n");
        let total = csr.fields.len();
        for f in csr.fields.iter().take(MAX_CSR_FIELDS) {
            let raw_bits = if is_rv32 {
                f.bits_rv32.as_deref().unwrap_or(&f.bits)
            } else {
                f.bits.as_str()
            };
            let bits = raw_bits.replace('-', ":");
            let fsum = f
                .summary
                .as_deref()
                .map(str::to_owned)
                .or_else(|| {
                    f.description
                        .as_deref()
                        .map(fallback_first_sentence)
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_default();
            if fsum.is_empty() {
                let _ = writeln!(list, "- `{}` [{bits}]", f.name);
            } else {
                let _ = writeln!(list, "- `{}` [{bits}] — {fsum}", f.name);
            }
        }
        if total > MAX_CSR_FIELDS {
            let _ = writeln!(list, "- *+{} more fields*", total - MAX_CSR_FIELDS);
        }
        facts.push(list.trim_end().to_owned());
    }

    let prose = csr
        .summary
        .clone()
        .or_else(|| {
            csr.description
                .as_deref()
                .map(fallback_first_sentence)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            csr.llvm_supported
                .then(|| "*No description in RISC-V specification.*".to_owned())
        });

    let mut notices = Vec::new();
    if !csr.llvm_supported {
        notices.push("Not accepted by LLVM 19 (spec only).".to_owned());
    } else if let Some(march) = effective_march
        && !march.supports_csr(&csr.required_extensions, csr.xlen)
    {
        if !march.supports_xlen(csr.xlen) {
            let xlen_name = match csr.xlen {
                Xlen::Rv32 => "RV32",
                Xlen::Rv64 => "RV64",
                Xlen::Both => "a different XLEN",
            };
            notices.push(format!(
                "Requires {xlen_name}; not available on `-march={}`.",
                march.raw
            ));
        } else {
            let reqs: Vec<String> = csr
                .required_extensions
                .iter()
                .map(|r| format_ext_req_notice(r))
                .collect();
            notices.push(format!(
                "Requires {}; not enabled by `-march={}`.",
                reqs.join(" + "),
                march.raw
            ));
        }
    }

    RenderedHoverParts {
        context,
        lang: "riscv",
        code: csr.name.clone(),
        below_fence: None,
        notices,
        facts,
        prose,
    }
    .render()
}

pub(crate) fn format_register_hover(reg: &RegisterEntry) -> String {
    let class_label = match reg.class {
        RegisterClass::Integer => "Integer",
        RegisterClass::Float => "Floating-point",
        RegisterClass::Vector => "Vector",
    };
    let mut header = vec!["Register".to_owned(), class_label.to_owned()];
    if let Some(saver) = &reg.saver {
        header.push(saver.clone());
    }
    let context = header.join(" · ");

    let code = if reg.abi_names.is_empty() {
        reg.name.clone()
    } else {
        format!("{} ({})", reg.abi_names.join(" / "), reg.name)
    };

    let prose = reg
        .summary
        .clone()
        .or_else(|| {
            reg.description
                .as_deref()
                .map(fallback_first_sentence)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            format!(
                "{} register #{}.",
                class_label.to_ascii_lowercase(),
                reg.number
            )
        });

    RenderedHoverParts {
        context,
        lang: "riscv",
        code,
        below_fence: None,
        notices: Vec::new(),
        facts: Vec::new(),
        prose: Some(prose),
    }
    .render()
}

pub(crate) fn format_directive_hover(dir: &DirectiveEntry) -> String {
    let scope = if dir.llvm_supported {
        "GNU / LLVM"
    } else {
        "GNU only"
    };
    let context = format!("Directive · {scope}");
    let code = match &dir.arguments {
        Some(args) => format!("{} {args}", dir.name),
        None => dir.name.clone(),
    };
    let mut notices = Vec::new();
    if !dir.llvm_supported {
        notices.push("Not accepted by LLVM 19 (GNU assembler / docs only).".to_owned());
    }
    let prose = dir
        .summary
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&dir.description);

    RenderedHoverParts {
        context,
        lang: "riscv",
        code,
        below_fence: None,
        notices,
        facts: Vec::new(),
        prose: Some(prose.to_owned()),
    }
    .render()
}

pub(crate) fn format_cpp_directive_hover(name: &str) -> String {
    let code = format!("#{name}");
    let desc = crate::preprocessor::directive_description(name).map(str::to_owned);
    let notices = vec![
        "In preprocessed assembly files, a `#` line starting with a directive name is handled by the C preprocessor, not treated as a comment. Use `//` for comments.".to_owned(),
    ];
    RenderedHoverParts {
        context: "Preprocessor directive".to_owned(),
        lang: "c",
        code,
        below_fence: None,
        notices,
        facts: Vec::new(),
        prose: desc,
    }
    .render()
}

pub(crate) fn format_include_hover(kind: IncludeKind, target: &str, resolved: &str) -> String {
    let (lang, code) = match kind {
        IncludeKind::CppQuote => ("c", format!("#include \"{target}\"")),
        IncludeKind::CppSystem => ("c", format!("#include <{target}>")),
        IncludeKind::AsmQuote => ("riscv", format!(".include \"{target}\"")),
    };
    RenderedHoverParts {
        context: "Include".to_owned(),
        lang,
        code,
        below_fence: None,
        notices: Vec::new(),
        facts: vec![format!("Path: `{resolved}`")],
        prose: None,
    }
    .render()
}

pub(crate) fn format_unresolved_include_hover(kind: IncludeKind, target: &str) -> String {
    let (lang, code) = match kind {
        IncludeKind::CppQuote => ("c", format!("#include \"{target}\"")),
        IncludeKind::CppSystem => ("c", format!("#include <{target}>")),
        IncludeKind::AsmQuote => ("riscv", format!(".include \"{target}\"")),
    };
    RenderedHoverParts {
        context: "Include · unresolved".to_owned(),
        lang,
        code,
        below_fence: None,
        notices: vec![
            "File not found in the active include search paths.".to_owned(),
        ],
        facts: Vec::new(),
        prose: Some(
            "Configure include directories (`-I` / `-isystem`) in `compile_commands.json` or `compile_flags.txt`."
                .to_owned(),
        ),
    }
    .render()
}

pub(crate) fn format_operand_keyword_hover(context: &str, code: &str, prose: &str) -> String {
    RenderedHoverParts {
        context: context.to_owned(),
        lang: "riscv",
        code: code.to_owned(),
        below_fence: None,
        notices: Vec::new(),
        facts: Vec::new(),
        prose: Some(prose.to_owned()),
    }
    .render()
}

pub(crate) fn format_macro_counter_hover(db: &Database) -> String {
    let mc = &db.texts.macro_counter;
    let title = if mc.title.is_empty() {
        "Macro invocation counter"
    } else {
        mc.title.as_str()
    };
    let sym = if mc.symbol.is_empty() {
        "\\@"
    } else {
        mc.symbol.as_str()
    };
    RenderedHoverParts {
        context: title.to_owned(),
        lang: "riscv",
        code: sym.to_owned(),
        below_fence: None,
        notices: Vec::new(),
        facts: Vec::new(),
        prose: (!mc.description.is_empty()).then(|| mc.description.clone()),
    }
    .render()
}

pub(crate) fn format_symbol_hover(info: &SymbolHoverInfo) -> String {
    let kind_label = match info.kind {
        SymKind::Label => "Label",
        SymKind::LocalLabel => "Local label",
        SymKind::NumericLabel => "Numeric label",
        SymKind::Equ => match info.assign_kind {
            Some(AssignKind::Set) => "Symbol (.set)",
            Some(AssignKind::Equals) => "Symbol (=)",
            Some(AssignKind::Equiv) => "Constant (.equiv)",
            Some(AssignKind::Equ) | None => "Constant (.equ)",
        },
        SymKind::Macro => "Macro",
        SymKind::MacroParam => match info.param_label.as_deref() {
            Some("macro parameter") | None => "Macro parameter",
            Some(other) => other,
        },
        SymKind::Define => "#define",
    };

    let mut header = vec![kind_label.to_owned()];
    if let Some(line) = info.target_line_1based {
        if let Some(uri) = &info.target_uri {
            let file_label = uri_to_path(uri)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| {
                    uri.as_str()
                        .rsplit('/')
                        .next()
                        .unwrap_or_else(|| uri.as_str())
                        .to_owned()
                });
            header.push(format!("`{file_label}:{line}`"));
        } else {
            header.push(format!("line {line}"));
        }
    }
    let context = header.join(" · ");

    let mut notices = Vec::new();
    if info.assignment_count > 1 {
        notices.push(format!(
            "Assigned {} times in scope; showing the assignment in effect here.",
            info.assignment_count
        ));
    }

    let mut facts = Vec::new();
    if let Some(val) = &info.value {
        facts.push(format!("Value: `{val}`"));
    }

    RenderedHoverParts {
        context,
        lang: "riscv",
        code: info.def_line.clone(),
        below_fence: None,
        notices,
        facts,
        prose: None,
    }
    .render()
}

fn extract_leading_bold_title(desc: &str) -> Option<&str> {
    let first = desc.split("\n\n").next()?.trim();
    let inner = first.strip_prefix("**")?.strip_suffix("**")?.trim();
    if inner.is_empty() || inner.contains("**") || inner.contains('\n') {
        None
    } else {
        Some(inner)
    }
}

fn fallback_first_sentence(desc: &str) -> String {
    let mut paragraphs = desc.split("\n\n").map(str::trim).filter(|p| !p.is_empty());
    let mut first_para = paragraphs.next().unwrap_or(desc);
    if extract_leading_bold_title(first_para).is_some()
        && let Some(next) = paragraphs.next()
    {
        first_para = next;
    }
    let flat = first_para.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut in_tick = false;
    for (idx, ch) in flat.char_indices() {
        if ch == '`' {
            in_tick = !in_tick;
        } else if ch == '.' && !in_tick {
            let next_ok = flat[idx + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
            if next_ok {
                return flat[..=idx].to_owned();
            }
        }
    }
    flat
}
