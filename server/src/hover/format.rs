use std::collections::HashSet;
use std::fmt::Write;

use crate::context::MarchSpec;
use crate::data::{
    CsrEntry, DirectiveEntry, InstructionEntry, InstructionKind, RegisterEntry, Xlen,
};
use crate::index::{AssignKind, SymKind, SymbolHoverInfo};

pub(super) fn format_instruction_hover(
    inst: &InstructionEntry,
    effective_march: &MarchSpec,
    enabled_by_march: bool,
) -> String {
    let mut md = String::new();

    let xlen_matching: Vec<_> = inst
        .variants
        .iter()
        .filter(|v| effective_march.supports_xlen(v.xlen))
        .collect();
    let variants_to_show = if xlen_matching.is_empty() {
        inst.variants.iter().collect()
    } else {
        xlen_matching
    };

    let mut seen_syntax = HashSet::new();
    let mut unique_syntaxes = Vec::new();
    for v in &variants_to_show {
        let norm = normalize_syntax(&v.syntax);
        if seen_syntax.insert(norm) {
            unique_syntaxes.push(v.syntax.as_str());
        }
    }
    if unique_syntaxes.is_empty() {
        let _ = writeln!(md, "```riscv\n{}\n```", inst.mnemonic);
    } else {
        let _ = writeln!(md, "```riscv");
        for syn in &unique_syntaxes {
            let _ = writeln!(md, "{syn}");
        }
        let _ = writeln!(md, "```");
    }

    // Filter out self-references (`ld` -> `ld`) and deduplicate targets.
    let mut seen_aliases = HashSet::new();
    let mut aliases = Vec::new();
    for v in &inst.variants {
        if let Some(alias) = v.alias_of.as_deref()
            && !alias.eq_ignore_ascii_case(&inst.mnemonic)
            && seen_aliases.insert(alias.to_ascii_lowercase())
        {
            aliases.push(alias);
        }
    }

    let mut exts = Vec::new();
    let mut has_rv32 = false;
    let mut has_rv64 = false;
    for v in &variants_to_show {
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

    let mut meta = Vec::new();
    if let Some(long_name) = &inst.long_name {
        meta.push(format!("**{long_name}**"));
    }
    match inst.kind {
        InstructionKind::Alias => {
            if aliases.is_empty() {
                meta.push("alias".to_owned());
            } else {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!("alias of {}", targets.join(", ")));
            }
        }
        InstructionKind::Pseudo => {
            if aliases.is_empty() {
                meta.push("pseudo-instruction".to_owned());
            } else {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!(
                    "pseudo-instruction · alias of {}",
                    targets.join(", ")
                ));
            }
        }
        InstructionKind::Instruction => {
            if inst.long_name.is_none() {
                meta.push(format!("**`{}`**", inst.mnemonic));
            }
            if !aliases.is_empty() {
                let targets: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
                meta.push(format!("alias of {}", targets.join(", ")));
            }
        }
    }

    let xlen_label = match (has_rv32, has_rv64) {
        (true, false) => "RV32",
        (false, true) => "RV64",
        _ => "RV32/RV64",
    };

    if exts.is_empty() {
        let base_badge = match (has_rv32, has_rv64) {
            (true, false) => "RV32I",
            (false, true) => "RV64I",
            _ => "I",
        };
        meta.push(format!("`{base_badge}`"));
    } else {
        let formatted: Vec<String> = exts.iter().map(|e| format!("`{e}`")).collect();
        meta.push(formatted.join(", "));
    }
    meta.push(xlen_label.to_owned());
    let _ = writeln!(md, "{}", meta.join(" · "));

    if !inst.llvm_supported {
        let _ = writeln!(md, "\n> Not accepted by LLVM 19 (spec only).");
    } else if !enabled_by_march {
        let notice = format_march_disabled_notice(inst, effective_march);
        let _ = writeln!(md, "\n> {notice}");
    }

    if inst.deprecated {
        if let Some(pref) = &inst.preferred_name {
            let _ = writeln!(md, "\n> Deprecated — prefer `{pref}`.");
        } else {
            let _ = writeln!(md, "\n> Deprecated.");
        }
    }

    if let Some(desc) = &inst.description {
        let first_para = desc.split("\n\n").next().unwrap_or(desc);
        let flat = first_para.split_whitespace().collect::<Vec<_>>().join(" ");
        if !flat.is_empty() {
            let _ = writeln!(md, "\n{}", truncate_chars_ellipsis(&flat, 300));
        }
    } else if inst.llvm_supported {
        let _ = writeln!(md, "\n*No description in RISC-V specification.*");
    }

    md.trim_end().to_owned()
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

pub(super) fn format_csr_hover(csr: &CsrEntry) -> String {
    let mut md = String::new();
    let mut header = vec![format!("`{}`", csr.name), format!("0x{:03x}", csr.number)];
    if let Some(priv_label) = csr.privilege_label() {
        header.push(priv_label.to_owned());
    }
    if !csr.llvm_supported {
        header.push("spec only".to_owned());
    }
    let _ = writeln!(md, "{}", header.join(" · "));

    if let Some(desc) = &csr.description {
        let summary = first_sentence_summary(desc);
        if !summary.is_empty() {
            let _ = writeln!(md, "\n{}", truncate_chars_ellipsis(&summary, 220));
        }
    } else if csr.llvm_supported {
        let _ = writeln!(md, "\n*No description in RISC-V specification.*");
    }

    if !csr.fields.is_empty() {
        md.push('\n');
        for f in &csr.fields {
            let bits = f.bits.replace('-', ":");
            let summary = f
                .description
                .as_deref()
                .map(first_sentence_summary)
                .unwrap_or_default();
            let truncated = truncate_chars_ellipsis(&summary, 80);
            if truncated.is_empty() {
                let _ = writeln!(md, "- `{}` [{bits}]", f.name);
            } else {
                let _ = writeln!(md, "- `{}` [{bits}] — {truncated}", f.name);
            }
        }
    }

    md.trim_end().to_owned()
}

pub(super) fn format_register_hover(reg: &RegisterEntry) -> String {
    let name_part = if reg.abi_names.is_empty() {
        format!("`{}`", reg.name)
    } else {
        let abis = reg
            .abi_names
            .iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(" / ");
        format!("{abis} ({})", reg.name)
    };

    let role_part = reg
        .description
        .as_deref()
        .map(first_sentence_summary)
        .map(|s| s.trim_end_matches('.').to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let cls = match reg.class {
                crate::data::RegisterClass::Integer => "integer register",
                crate::data::RegisterClass::Float => "floating-point register",
                crate::data::RegisterClass::Vector => "vector register",
            };
            format!("{cls} #{}", reg.number)
        });

    let mut parts = vec![name_part, truncate_chars_ellipsis(&role_part, 100)];
    if let Some(saver) = &reg.saver {
        parts.push(saver.clone());
    }
    parts.join(" · ")
}

pub(super) fn format_directive_hover(dir: &DirectiveEntry) -> String {
    let mut md = String::new();
    if let Some(args) = &dir.arguments {
        let _ = writeln!(md, "```riscv\n{} {args}\n```", dir.name);
    } else {
        let _ = writeln!(md, "```riscv\n{}\n```", dir.name);
    }

    if !dir.llvm_supported {
        let _ = writeln!(
            md,
            "\n> Not accepted by LLVM 19 (GNU assembler / docs only)."
        );
    }

    let _ = writeln!(md, "\n{}", dir.description);
    md.trim_end().to_owned()
}

pub(super) fn format_symbol_hover(info: &SymbolHoverInfo) -> String {
    let mut md = String::new();
    let kind_label = match info.kind {
        SymKind::Label => "label",
        SymKind::LocalLabel => "local label",
        SymKind::NumericLabel => "numeric label",
        SymKind::Equ => match info.assign_kind {
            Some(AssignKind::Set) => "symbol (.set)",
            Some(AssignKind::Equals) => "symbol (=)",
            Some(AssignKind::Equiv) => "constant (.equiv)",
            Some(AssignKind::Equ) | None => "constant (.equ)",
        },
        SymKind::Macro => "macro",
        SymKind::MacroParam => info.param_label.as_deref().unwrap_or("macro parameter"),
        SymKind::Define => "#define",
    };

    let mut header = vec![format!("`{}`", info.name), kind_label.to_owned()];
    if let Some(line) = info.target_line_1based {
        if let Some(uri) = &info.target_uri {
            let file_label = uri
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_else(|| uri.as_str());
            header.push(format!("`{file_label}:{line}`"));
        } else {
            header.push(format!("line {line}"));
        }
    }
    if let Some(val) = &info.value {
        header.push(format!("`{val}`"));
    }

    let _ = writeln!(md, "{}", header.join(" · "));
    let _ = writeln!(md, "\n```riscv\n{}\n```", info.def_line);
    if info.assignment_count > 1 {
        let _ = writeln!(
            md,
            "\n*Assigned {} times in scope; showing the assignment in effect here.*",
            info.assignment_count
        );
    }
    md.trim_end().to_owned()
}

fn first_sentence_summary(desc: &str) -> String {
    let first_para = desc.split("\n\n").next().unwrap_or(desc);
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

fn truncate_chars_ellipsis(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars || max_chars == 0 {
        return s.to_owned();
    }
    let cut_limit = max_chars.saturating_sub(1);
    let mut end_idx = s.len();
    for (count, (idx, _)) in s.char_indices().enumerate() {
        if count >= cut_limit {
            end_idx = idx;
            break;
        }
    }
    let slice = &s[..end_idx];
    let trimmed = if let Some(ws) = slice.rfind(char::is_whitespace)
        && ws > cut_limit / 2
    {
        &slice[..ws]
    } else {
        slice
    };
    format!("{}…", trimmed.trim_end())
}
