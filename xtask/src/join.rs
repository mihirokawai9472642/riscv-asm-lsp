use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use riscv_asm_lsp::data::{
    CsrEntry, DataMeta, DirectiveEntry, InstructionEntry, InstructionKind, InstructionVariant,
    OperandSpec, RegisterEntry, RiscvData, Xlen,
};
use serde::Deserialize;

use crate::asm_manual::AsmManualData;
use crate::llvm::LlvmData;
use crate::pins;
use crate::udb::UdbData;

#[derive(Debug, Clone)]
pub struct JoinReportData {
    pub llvm_only_with_manual: Vec<String>,
    pub llvm_only_with_supplement: Vec<String>,
    pub llvm_only_no_desc: Vec<String>,
    pub udb_only_instructions: Vec<String>,
    pub both_instructions: usize,
    pub missing_rv64gc: Vec<String>,
    pub llvm_only_csrs_with_supplement: Vec<(String, u16)>,
    pub llvm_only_csrs: Vec<(String, u16)>,
    pub udb_only_csrs: Vec<(String, u16)>,
    pub both_csrs: usize,
    pub deprecated_joined: Vec<(String, String)>,
    pub description_corrections: Vec<(String, String, String)>,
    pub cleaned_todos: Vec<String>,
    pub udb_copyrights: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementsFile {
    source: String,
    instructions: Vec<SupplementalInst>,
    csrs: Vec<SupplementalCsr>,
    #[serde(default)]
    directives: Vec<SupplementalDirective>,
    description_corrections: Vec<DescriptionCorrection>,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementalDirective {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementalInst {
    mnemonic: String,
    description: String,
    #[serde(default)]
    summary: Option<String>,
    deprecated: bool,
    preferred_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementalCsr {
    name: String,
    #[serde(default)]
    required_extensions: Vec<String>,
    description: String,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DescriptionCorrection {
    owner: String,
    wrong: String,
    right: String,
}

fn load_supplements(workspace_root: &Path) -> Result<SupplementsFile, Box<dyn std::error::Error>> {
    let path = workspace_root.join("server/data/supplements.json");
    let text = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

#[derive(Debug, Deserialize)]
struct HandGnuDirectivesFile {
    source: String,
    directives: Vec<HandGnuDirective>,
}

#[derive(Debug, Deserialize)]
struct HandGnuDirective {
    name: String,
    arguments: Option<String>,
    description: String,
}

pub fn build(
    workspace_root: &Path,
    llvm: LlvmData,
    udb: UdbData,
    manual: AsmManualData,
) -> Result<(RiscvData, JoinReportData), Box<dyn std::error::Error>> {
    let supplements = load_supplements(workspace_root)?;
    let mut description_corrections = Vec::new();
    let (instructions, inst_report) = join_instructions(
        &llvm,
        &udb,
        &manual,
        &supplements,
        &mut description_corrections,
    );
    let (csrs, csr_report) = join_csrs(&llvm, &udb, &supplements, &mut description_corrections);
    let registers = join_registers(&llvm, &udb);
    let directives = join_directives(workspace_root, &manual, &supplements)?;

    let data = RiscvData {
        meta: DataMeta {
            generated: true,
            llvm_tag: pins::LLVM_TAG.to_owned(),
            udb_commit: pins::UDB_COMMIT.to_owned(),
            asm_manual_commit: pins::ASM_MANUAL_COMMIT.to_owned(),
        },
        extensions: llvm.extensions,
        instructions,
        csrs,
        registers,
        directives,
    };

    let report = JoinReportData {
        llvm_only_with_manual: inst_report.llvm_only_with_manual,
        llvm_only_with_supplement: inst_report.llvm_only_with_supplement,
        llvm_only_no_desc: inst_report.llvm_only_no_desc,
        udb_only_instructions: inst_report.udb_only,
        both_instructions: inst_report.both,
        missing_rv64gc: inst_report.missing_rv64gc,
        llvm_only_csrs_with_supplement: csr_report.llvm_only_with_supplement,
        llvm_only_csrs: csr_report.llvm_only,
        udb_only_csrs: csr_report.udb_only,
        both_csrs: csr_report.both,
        deprecated_joined: csr_report.deprecated_joined,
        description_corrections,
        cleaned_todos: udb.cleaned_todos,
        udb_copyrights: udb.copyrights,
    };

    Ok((data, report))
}

struct InstReport {
    both: usize,
    llvm_only_with_manual: Vec<String>,
    llvm_only_with_supplement: Vec<String>,
    llvm_only_no_desc: Vec<String>,
    udb_only: Vec<String>,
    missing_rv64gc: Vec<String>,
}

fn apply_description_corrections(
    owner: &str,
    mut desc: String,
    correction_table: &[DescriptionCorrection],
    recorded: &mut Vec<(String, String, String)>,
) -> String {
    for c in correction_table {
        if owner == c.owner && desc.contains(&c.wrong) {
            desc = desc.replacen(&c.wrong, &c.right, 1);
            recorded.push((owner.to_owned(), c.wrong.clone(), c.right.clone()));
        }
    }
    desc
}

const SUMMARY_ENTRY_MAX_CHARS: usize = 260;
const SUMMARY_SINGLE_SENTENCE_MAX_CHARS: usize = 320;
const SUMMARY_FIELD_MAX_CHARS: usize = 140;

/// Extract a deterministic 1–2 sentence summary for display without runtime truncation.
pub fn make_summary(desc: &str, max_chars: usize) -> Option<String> {
    make_summary_with_limit(desc, max_chars, 2)
}

fn make_field_summary(desc: &str, max_chars: usize) -> Option<String> {
    make_summary_with_limit(desc, max_chars, 1)
}

fn make_summary_with_limit(desc: &str, max_chars: usize, max_sentences: usize) -> Option<String> {
    let paras: Vec<&str> = desc
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let mut idx = 0usize;
    while idx + 1 < paras.len()
        && (is_bold_heading(paras[idx])
            || paras[idx].starts_with('|')
            || paras[idx].starts_with("**Note:**"))
    {
        idx += 1;
    }
    // Skip `...: \n\n *See the RISC-V ISA specification...*` table placeholders.
    while idx + 2 < paras.len()
        && paras[idx].trim_end().ends_with(':')
        && paras[idx + 1].starts_with("*See the RISC-V ISA specification")
    {
        idx += 2;
    }
    let chosen = *paras.get(idx)?;
    if is_bold_heading(chosen) || chosen.starts_with('|') {
        return None;
    }

    let mut flat = chosen.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    if let Some(stripped) = strip_leading_syntax_prefix(&flat) {
        flat = stripped.to_owned();
    }

    if flat.ends_with(':')
        && let Some(next_para) = paras.get(idx + 1)
        && let Some(joined) = join_colon_list(&flat, next_para)
    {
        let cap = if max_sentences > 1 {
            max_chars.max(SUMMARY_SINGLE_SENTENCE_MAX_CHARS)
        } else {
            max_chars
        };
        return Some(finalize_summary(&joined, cap));
    }

    let sentences = split_sentences(&flat);
    let (selected, cap) = if let Some(first) = sentences.first() {
        if max_sentences > 1
            && let Some(second) = sentences.get(1)
        {
            let combined = format!("{first} {second}");
            if combined.chars().count() <= max_chars {
                (combined, max_chars)
            } else {
                (
                    (*first).to_owned(),
                    max_chars.max(SUMMARY_SINGLE_SENTENCE_MAX_CHARS),
                )
            }
        } else {
            let single_cap = if max_sentences > 1 {
                max_chars.max(SUMMARY_SINGLE_SENTENCE_MAX_CHARS)
            } else {
                max_chars
            };
            ((*first).to_owned(), single_cap)
        }
    } else {
        (flat, max_chars)
    };

    Some(finalize_summary(&selected, cap))
}

fn finalize_summary(raw: &str, max_chars: usize) -> String {
    let mut s = raw.trim().to_owned();
    if let Some(inner) = s.strip_prefix('*').and_then(|r| r.strip_suffix('*'))
        && !inner.is_empty()
        && !inner.contains('*')
    {
        s = inner.trim().to_owned();
    }
    let truncated = truncate_clean_markdown(&s, max_chars);
    if !truncated.ends_with('…')
        && truncated
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '`' | '_' | ')'))
    {
        format!("{truncated}.")
    } else {
        truncated
    }
}

fn join_colon_list(intro: &str, list_para: &str) -> Option<String> {
    let lines: Vec<&str> = list_para
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut items: Vec<String> = Vec::new();
    for line in lines {
        if let Some(sub) = line.strip_prefix("** ") {
            let clean_sub = sub.trim().trim_end_matches(['.', ';']);
            if let Some(prev) = items.last_mut()
                && !clean_sub.is_empty()
            {
                prev.push_str(&format!(" ({clean_sub})"));
            } else {
                return None;
            }
        } else {
            let body = line
                .strip_prefix("* ")
                .or_else(|| line.strip_prefix("- "))?;
            let clean = body.trim().trim_end_matches(['.', ';']);
            if !clean.is_empty() {
                items.push(lowercase_sentence_start(clean));
            }
        }
    }
    if items.is_empty() {
        return None;
    }
    Some(format!("{intro} {}.", items.join("; ")))
}

fn lowercase_sentence_start(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let Some(second) = chars.next() else {
        return s.to_owned();
    };
    if first.is_ascii_uppercase() && second.is_ascii_lowercase() {
        let mut out = String::with_capacity(s.len());
        out.push(first.to_ascii_lowercase());
        out.push(second);
        out.push_str(chars.as_str());
        out
    } else {
        s.to_owned()
    }
}

fn strip_leading_syntax_prefix(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('`')?;
    let end_tick = rest.find('`')?;
    rest[end_tick + 1..].trim_start().strip_prefix("— ")
}

fn is_bold_heading(p: &str) -> bool {
    let t = p.trim();
    t.starts_with("**")
        && t.ends_with("**")
        && !t.contains('\n')
        && !t[2..t.len() - 2].contains("**")
}

fn split_sentences(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_tick = false;
    for (idx, ch) in s.char_indices() {
        if ch == '`' {
            in_tick = !in_tick;
        } else if ch == '.' && !in_tick {
            let next_ok = s[idx + 1..].chars().next().is_none_or(char::is_whitespace);
            let before = &s[start..=idx];
            let is_abbrev = ["e.g.", "i.e.", "etc.", "vs.", "Sec."]
                .iter()
                .any(|ab| before.ends_with(ab));
            if next_ok && !is_abbrev {
                let piece = s[start..=idx].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = idx + 1;
            }
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn truncate_clean_markdown(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars || max_chars == 0 {
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
    }
    .trim_end();

    let mut out = trimmed.to_owned();
    if out.matches('`').count() % 2 == 1
        && let Some(last_tick) = out.rfind('`')
    {
        out = out[..last_tick].trim_end().to_owned();
    }
    let clean = out.trim_end_matches([' ', '\t', ':', ';', ',']);
    format!("{clean}…")
}

fn join_instructions(
    llvm: &LlvmData,
    udb: &UdbData,
    manual: &AsmManualData,
    supplements: &SupplementsFile,
    corrections: &mut Vec<(String, String, String)>,
) -> (Vec<InstructionEntry>, InstReport) {
    let mut llvm_grouped: BTreeMap<String, Vec<&crate::llvm::LlvmInstruction>> = BTreeMap::new();
    for inst in &llvm.instructions {
        llvm_grouped
            .entry(inst.mnemonic.clone())
            .or_default()
            .push(inst);
    }
    let udb_by_name = index_udb_instructions(udb);
    let manual_by_name: HashMap<String, &crate::asm_manual::ManualPseudo> = manual
        .pseudos
        .iter()
        .map(|p| (p.mnemonic.clone(), p))
        .collect();
    let supplement_by_name: HashMap<&str, &SupplementalInst> = supplements
        .instructions
        .iter()
        .map(|s| (s.mnemonic.as_str(), s))
        .collect();

    let mut report = InstReport {
        both: 0,
        llvm_only_with_manual: Vec::new(),
        llvm_only_with_supplement: Vec::new(),
        llvm_only_no_desc: Vec::new(),
        udb_only: Vec::new(),
        missing_rv64gc: Vec::new(),
    };
    let mut out = Vec::new();
    let mut seen_udb = BTreeSet::new();

    let sources = InstJoinSources {
        udb_by_name: &udb_by_name,
        manual_by_name: &manual_by_name,
        supplement_by_name: &supplement_by_name,
        supplements,
    };

    for (mnemonic, group) in &llvm_grouped {
        let entry = build_llvm_instruction_entry(
            mnemonic,
            group,
            &sources,
            corrections,
            &mut seen_udb,
            &mut report,
        );
        out.push(entry);
    }

    append_udb_only_instructions(udb_by_name, &llvm_grouped, &seen_udb, &mut out, &mut report);

    (out, report)
}

fn index_udb_instructions(udb: &UdbData) -> HashMap<String, &crate::udb::UdbInstruction> {
    let mut map: HashMap<String, &crate::udb::UdbInstruction> = HashMap::new();
    for inst in &udb.instructions {
        map.entry(inst.name.clone())
            .and_modify(|existing| {
                if existing.is_pseudo && !inst.is_pseudo {
                    *existing = inst;
                }
            })
            .or_insert(inst);
    }
    map
}

struct InstJoinSources<'a> {
    udb_by_name: &'a HashMap<String, &'a crate::udb::UdbInstruction>,
    manual_by_name: &'a HashMap<String, &'a crate::asm_manual::ManualPseudo>,
    supplement_by_name: &'a HashMap<&'a str, &'a SupplementalInst>,
    supplements: &'a SupplementsFile,
}

fn build_llvm_instruction_entry(
    mnemonic: &str,
    group: &[&crate::llvm::LlvmInstruction],
    sources: &InstJoinSources<'_>,
    corrections: &mut Vec<(String, String, String)>,
    seen_udb: &mut BTreeSet<String>,
    report: &mut InstReport,
) -> InstructionEntry {
    let mut variants = Vec::new();
    let mut seen_variants = BTreeSet::new();
    let mut has_instruction = false;
    let mut has_pseudo = false;
    for item in group {
        match item.kind {
            InstructionKind::Instruction => has_instruction = true,
            InstructionKind::Pseudo => has_pseudo = true,
            InstructionKind::Alias => {}
        }
        let key = format!(
            "{}|{:?}|{}",
            item.variant.syntax,
            item.variant.xlen,
            item.variant.required_extensions.join(",")
        );
        if seen_variants.insert(key) {
            variants.push(item.variant.clone());
        }
    }
    let kind = if has_instruction {
        InstructionKind::Instruction
    } else if has_pseudo {
        InstructionKind::Pseudo
    } else {
        InstructionKind::Alias
    };

    let udb_entry = sources.udb_by_name.get(mnemonic).copied();
    let manual_entry = sources.manual_by_name.get(mnemonic).copied();
    let supplement_entry = sources.supplement_by_name.get(mnemonic).copied();

    if udb_entry.is_some() {
        seen_udb.insert(mnemonic.to_owned());
        report.both += 1;
    } else if manual_entry.is_some() {
        report.llvm_only_with_manual.push(mnemonic.to_owned());
    } else if supplement_entry.is_some() {
        report.llvm_only_with_supplement.push(mnemonic.to_owned());
    } else {
        report.llvm_only_no_desc.push(mnemonic.to_owned());
    }

    let udb_assembly = udb_entry.and_then(|u| u.assembly.clone());
    let isa_label = derive_isa_label(mnemonic, &variants, udb_entry);
    let (long_name, raw_description, description_source) = resolve_inst_docs(
        mnemonic,
        udb_entry,
        manual_entry,
        supplement_entry,
        sources.supplements,
        corrections,
    );
    let description = raw_description.map(|d| normalize_udb_register_names(&d, &variants));
    let summary = supplement_entry
        .and_then(|s| s.summary.clone())
        .or_else(|| {
            description
                .as_deref()
                .and_then(|d| make_summary(d, SUMMARY_ENTRY_MAX_CHARS))
        });
    let (deprecated, preferred_name) = supplement_entry
        .map(|s| (s.deprecated, s.preferred_name.clone()))
        .unwrap_or((false, None));

    InstructionEntry {
        mnemonic: mnemonic.to_owned(),
        kind,
        variants,
        udb_assembly,
        isa_label,
        long_name,
        summary,
        description,
        description_source,
        llvm_supported: true,
        deprecated,
        preferred_name,
    }
}

fn derive_isa_label(
    mnemonic: &str,
    variants: &[InstructionVariant],
    udb_entry: Option<&crate::udb::UdbInstruction>,
) -> Option<String> {
    if variants.iter().any(|v| !v.required_extensions.is_empty()) {
        return None;
    }
    if matches!(
        mnemonic,
        "rdinstret" | "rdinstreth" | "rdcycle" | "rdcycleh" | "rdtime" | "rdtimeh"
    ) {
        return Some("Zicntr".to_owned());
    }
    let u = udb_entry?;
    let mut labels = Vec::new();
    for ext in &u.extensions {
        for alt in ext.split('|') {
            let trimmed = alt.trim();
            let lower = trimmed.to_ascii_lowercase();
            // Only preserve privileged/system/CSR/counter labels here; base RV32/RV64
            // instructions like ld/sd have noisy UDB extensions (e.g. Zilsd) that should
            // not override RV64I.
            if !matches!(
                lower.as_str(),
                "sm" | "s" | "sdext" | "zicsr" | "zifencei" | "zicntr" | "zihpm" | "smrnmi"
            ) {
                continue;
            }
            let formatted = format_ext_title(trimmed);
            if !labels.contains(&formatted) {
                labels.push(formatted);
            }
        }
    }
    (!labels.is_empty()).then(|| labels.join(" / "))
}

fn format_ext_title(ext: &str) -> String {
    let mut chars = ext.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn normalize_udb_register_names(text: &str, variants: &[InstructionVariant]) -> String {
    let has_op = |name: &str| {
        variants
            .iter()
            .any(|v| v.operands.iter().any(|o| o.name == name))
    };
    let mut out = text.to_owned();
    for (udb_name, llvm_name) in [("xd", "rd"), ("xs1", "rs1"), ("xs2", "rs2"), ("xs3", "rs3")] {
        if has_op(llvm_name) && !has_op(udb_name) {
            out = replace_reg_token(&out, udb_name, llvm_name);
        }
    }
    out
}

fn replace_reg_token(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if text[i..].starts_with(from) {
            let left_ok = i == 0
                || (!bytes[i - 1].is_ascii_alphanumeric()
                    && !(bytes[i - 1] == b'_' && i >= 2 && bytes[i - 2].is_ascii_alphanumeric()));
            let end = i + from.len();
            let right_ok = end >= bytes.len()
                || (!bytes[end].is_ascii_alphanumeric()
                    && !(bytes[end] == b'_'
                        && end + 1 < bytes.len()
                        && bytes[end + 1].is_ascii_alphanumeric()));
            if left_ok && right_ok {
                out.push_str(to);
                i = end;
                continue;
            }
        }
        if let Some(ch) = text[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn resolve_inst_docs(
    mnemonic: &str,
    udb_entry: Option<&crate::udb::UdbInstruction>,
    manual_entry: Option<&crate::asm_manual::ManualPseudo>,
    supplement_entry: Option<&SupplementalInst>,
    supplements: &SupplementsFile,
    corrections: &mut Vec<(String, String, String)>,
) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(u) = udb_entry {
        let desc = u.description.clone().map(|d| {
            apply_description_corrections(
                mnemonic,
                d,
                &supplements.description_corrections,
                corrections,
            )
        });
        let src = desc
            .as_ref()
            .map(|_| "riscv-unified-db (BSD-3-Clause-Clear)".to_owned());
        (u.long_name.clone(), desc, src)
    } else if let Some(m) = manual_entry {
        let formatted = if mnemonic == "li" || m.expansion == "Multiple instruction sequences" {
            format!(
                "`{}` — {} (expands to one or more instructions depending on the value)",
                m.syntax, m.description
            )
        } else {
            format!(
                "`{}` — {} (expands to `{}`)",
                m.syntax, m.description, m.expansion
            )
        };
        (
            None,
            Some(formatted),
            Some("riscv-asm-manual (CC-BY-4.0)".to_owned()),
        )
    } else if let Some(supp) = supplement_entry {
        (
            None,
            Some(supp.description.clone()),
            Some(supplements.source.clone()),
        )
    } else {
        (None, None, None)
    }
}

fn append_udb_only_instructions(
    udb_by_name: HashMap<String, &crate::udb::UdbInstruction>,
    llvm_grouped: &BTreeMap<String, Vec<&crate::llvm::LlvmInstruction>>,
    seen_udb: &BTreeSet<String>,
    out: &mut Vec<InstructionEntry>,
    report: &mut InstReport,
) {
    let rv64gc_exts: BTreeSet<&str> = [
        "i", "m", "a", "f", "d", "c", "zca", "zcd", "zicsr", "zifencei",
    ]
    .into_iter()
    .collect();

    let mut udb_sorted: Vec<_> = udb_by_name.into_iter().collect();
    udb_sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (mnemonic, u) in udb_sorted {
        if llvm_grouped.contains_key(&mnemonic) || seen_udb.contains(&mnemonic) {
            continue;
        }
        report.udb_only.push(mnemonic.clone());

        if !u.is_pseudo
            && u.extensions
                .iter()
                .any(|ext| ext.split('|').any(|part| rv64gc_exts.contains(part)))
        {
            report.missing_rv64gc.push(mnemonic.clone());
        }

        let kind = if u.is_pseudo {
            InstructionKind::Pseudo
        } else {
            InstructionKind::Instruction
        };
        let syntax = u.assembly.clone().unwrap_or_else(|| mnemonic.clone());
        let summary = u
            .description
            .as_deref()
            .and_then(|d| make_summary(d, SUMMARY_ENTRY_MAX_CHARS));
        let isa_label = derive_isa_label(&mnemonic, &[], Some(u));
        out.push(InstructionEntry {
            mnemonic,
            kind,
            variants: vec![InstructionVariant {
                syntax,
                operands: Vec::<OperandSpec>::new(),
                required_extensions: u.extensions.clone(),
                xlen: u.xlen,
                alias_of: u.alias_of.clone(),
                encoding: None,
                alias_expansion: None,
            }],
            udb_assembly: u.assembly.clone(),
            isa_label,
            long_name: u.long_name.clone(),
            summary,
            description: u.description.clone(),
            description_source: u
                .description
                .as_ref()
                .map(|_| "riscv-unified-db (BSD-3-Clause-Clear)".to_owned()),
            llvm_supported: false,
            deprecated: false,
            preferred_name: None,
        });
    }
}

struct CsrReport {
    both: usize,
    llvm_only_with_supplement: Vec<(String, u16)>,
    llvm_only: Vec<(String, u16)>,
    udb_only: Vec<(String, u16)>,
    deprecated_joined: Vec<(String, String)>,
}

fn summarize_csr_fields(
    mut fields: Vec<riscv_asm_lsp::data::CsrField>,
) -> Vec<riscv_asm_lsp::data::CsrField> {
    for f in &mut fields {
        f.summary = f
            .description
            .as_deref()
            .and_then(|d| make_field_summary(d, SUMMARY_FIELD_MAX_CHARS));
    }
    fields
}

fn join_csrs(
    llvm: &LlvmData,
    udb: &UdbData,
    supplements: &SupplementsFile,
    corrections: &mut Vec<(String, String, String)>,
) -> (Vec<CsrEntry>, CsrReport) {
    let mut udb_by_name: HashMap<String, &crate::udb::UdbCsr> = HashMap::new();
    for csr in &udb.csrs {
        udb_by_name.insert(csr.name.clone(), csr);
    }
    let supplement_csrs: HashMap<&str, &SupplementalCsr> = supplements
        .csrs
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    let mut report = CsrReport {
        both: 0,
        llvm_only_with_supplement: Vec::new(),
        llvm_only: Vec::new(),
        udb_only: Vec::new(),
        deprecated_joined: Vec::new(),
    };
    let mut out = Vec::new();
    let mut matched_udb_names = BTreeSet::new();
    let mut llvm_sorted = llvm.csrs.clone();
    llvm_sorted.sort_by(|a, b| a.number.cmp(&b.number).then_with(|| a.name.cmp(&b.name)));

    for l in &llvm_sorted {
        let entry = build_llvm_csr_entry(
            l,
            &udb_by_name,
            &supplement_csrs,
            supplements,
            corrections,
            &mut matched_udb_names,
            &mut report,
        );
        out.push(entry);
    }

    append_udb_only_csrs(udb, &matched_udb_names, &mut out, &mut report);

    (out, report)
}

fn build_llvm_csr_entry(
    l: &crate::llvm::LlvmCsr,
    udb_by_name: &HashMap<String, &crate::udb::UdbCsr>,
    supplement_csrs: &HashMap<&str, &SupplementalCsr>,
    supplements: &SupplementsFile,
    corrections: &mut Vec<(String, String, String)>,
    matched_udb_names: &mut BTreeSet<String>,
    report: &mut CsrReport,
) -> CsrEntry {
    let mut deprecated_names = Vec::new();
    if let Some(alt) = &l.alt_name {
        deprecated_names.push(alt.clone());
    }
    if let Some(dep) = &l.deprecated_name
        && !deprecated_names.contains(dep)
    {
        deprecated_names.push(dep.clone());
    }

    let udb_match = udb_by_name
        .get(&l.name)
        .copied()
        .or_else(|| {
            l.deprecated_name.as_deref().and_then(|dep| {
                let found = udb_by_name.get(dep).copied();
                if found.is_some() {
                    report
                        .deprecated_joined
                        .push((l.name.clone(), dep.to_owned()));
                }
                found
            })
        })
        .or_else(|| {
            l.alt_name
                .as_deref()
                .and_then(|alt| udb_by_name.get(alt).copied())
        });

    let xlen = if l.is_rv32_only {
        Xlen::Rv32
    } else {
        Xlen::Both
    };
    if let Some(u) = udb_match {
        matched_udb_names.insert(u.name.clone());
        report.both += 1;
        let raw_desc = match (&u.long_name, &u.description) {
            (Some(ln), Some(d)) => Some(format!("**{ln}**\n\n{d}")),
            (Some(ln), None) => Some(ln.clone()),
            (None, Some(d)) => Some(d.clone()),
            (None, None) => None,
        };
        let description = raw_desc.map(|d| {
            apply_description_corrections(
                &l.name,
                d,
                &supplements.description_corrections,
                corrections,
            )
        });
        let description_source = description
            .as_ref()
            .map(|_| "riscv-unified-db (BSD-3-Clause-Clear)".to_owned());
        let summary = supplement_csrs
            .get(l.name.as_str())
            .and_then(|s| s.summary.clone())
            .or_else(|| {
                description
                    .as_deref()
                    .and_then(|d| make_summary(d, SUMMARY_ENTRY_MAX_CHARS))
            });
        CsrEntry {
            name: l.name.clone(),
            number: l.number,
            privilege: u.privilege.clone(),
            required_extensions: u.extensions.clone(),
            fields: summarize_csr_fields(u.fields.clone()),
            summary,
            description,
            description_source,
            deprecated_names,
            xlen,
            llvm_supported: true,
        }
    } else {
        let supp = supplement_csrs.get(l.name.as_str()).copied();
        let supp_desc = supp.map(|s| s.description.clone());
        let summary = supp.and_then(|s| s.summary.clone()).or_else(|| {
            supp_desc
                .as_deref()
                .and_then(|d| make_summary(d, SUMMARY_ENTRY_MAX_CHARS))
        });
        let description_source = if supp_desc.is_some() {
            report
                .llvm_only_with_supplement
                .push((l.name.clone(), l.number));
            Some(supplements.source.clone())
        } else {
            report.llvm_only.push((l.name.clone(), l.number));
            None
        };
        let required_extensions = supp
            .map(|s| s.required_extensions.clone())
            .unwrap_or_default();
        CsrEntry {
            name: l.name.clone(),
            number: l.number,
            privilege: infer_csr_privilege(&l.name, l.number),
            required_extensions,
            fields: Vec::new(),
            summary,
            description: supp_desc,
            description_source,
            deprecated_names,
            xlen,
            llvm_supported: true,
        }
    }
}

fn append_udb_only_csrs(
    udb: &UdbData,
    matched_udb_names: &BTreeSet<String>,
    out: &mut Vec<CsrEntry>,
    report: &mut CsrReport,
) {
    let mut udb_sorted = udb.csrs.clone();
    udb_sorted.sort_by(|a, b| a.address.cmp(&b.address).then_with(|| a.name.cmp(&b.name)));
    for u in udb_sorted {
        if matched_udb_names.contains(&u.name) {
            continue;
        }
        report.udb_only.push((u.name.clone(), u.address));
        let description = match (&u.long_name, &u.description) {
            (Some(ln), Some(d)) => Some(format!("**{ln}**\n\n{d}")),
            (Some(ln), None) => Some(ln.clone()),
            (None, Some(d)) => Some(d.clone()),
            (None, None) => None,
        };
        let description_source = description
            .as_ref()
            .map(|_| "riscv-unified-db (BSD-3-Clause-Clear)".to_owned());
        let summary = description
            .as_deref()
            .and_then(|d| make_summary(d, SUMMARY_ENTRY_MAX_CHARS));
        let xlen = if u.name.ends_with('h')
            && (u.name.starts_with("cycle")
                || u.name.starts_with("time")
                || u.name.starts_with("instret")
                || u.name.starts_with("hpmcounter")
                || u.name.starts_with("mcycle")
                || u.name.starts_with("minstret")
                || u.name.starts_with("mhpmcounter")
                || u.name == "mstatush"
                || u.name == "menvcfgh"
                || u.name == "mseccfgh"
                || u.name.starts_with("mstateen")
                || u.name.starts_with("hstateen")
                || u.name == "medelegh"
                || u.name == "hedelegh")
        {
            Xlen::Rv32
        } else {
            Xlen::Both
        };
        out.push(CsrEntry {
            name: u.name,
            number: u.address,
            privilege: u.privilege,
            required_extensions: u.extensions,
            fields: summarize_csr_fields(u.fields),
            summary,
            description,
            description_source,
            deprecated_names: Vec::new(),
            xlen,
            llvm_supported: false,
        });
    }
}

fn infer_csr_privilege(name: &str, number: u16) -> Option<String> {
    let priv_bits = (number >> 8) & 0b11;
    let label = match priv_bits {
        0b00 => "U",
        0b01 => "S",
        0b10 => {
            if name.starts_with("vs") {
                "VS"
            } else if name.starts_with('h') {
                "HS"
            } else {
                "VS/HS"
            }
        }
        0b11 => "M",
        _ => return None,
    };
    Some(label.to_owned())
}

fn join_registers(llvm: &LlvmData, udb: &UdbData) -> Vec<RegisterEntry> {
    llvm.registers
        .iter()
        .map(|reg| {
            let mut merged = reg.clone();
            if let Some(info) = udb.registers.get(&reg.name) {
                merged.description = info.description.clone();
                merged.summary = info
                    .description
                    .as_deref()
                    .and_then(|d| make_summary(d, SUMMARY_FIELD_MAX_CHARS));
                merged.saver = info.saver.clone();
            }
            merged
        })
        .collect()
}

fn join_directives(
    workspace_root: &Path,
    manual: &AsmManualData,
    supplements: &SupplementsFile,
) -> Result<Vec<DirectiveEntry>, Box<dyn std::error::Error>> {
    let mut by_name: BTreeMap<String, DirectiveEntry> = BTreeMap::new();
    for dir in &manual.directives {
        let mut entry = dir.clone();
        entry.summary = make_summary(&entry.description, SUMMARY_ENTRY_MAX_CHARS);
        by_name.insert(entry.name.clone(), entry);
    }

    let hand_path = workspace_root.join("server/data/directives_gnu.json");
    let hand_text = fs::read_to_string(&hand_path)?;
    let hand: HandGnuDirectivesFile = serde_json::from_str(&hand_text)?;
    for dir in hand.directives {
        let name = dir.name.to_ascii_lowercase();
        let summary = make_summary(&dir.description, SUMMARY_ENTRY_MAX_CHARS);
        by_name.entry(name.clone()).or_insert(DirectiveEntry {
            name,
            arguments: dir.arguments,
            summary,
            description: dir.description,
            source: hand.source.clone(),
            llvm_supported: true,
        });
    }

    for sup in &supplements.directives {
        let name = sup.name.to_ascii_lowercase();
        let summary = make_summary(&sup.description, SUMMARY_ENTRY_MAX_CHARS);
        if let Some(entry) = by_name.get_mut(&name) {
            if let Some(args) = &sup.arguments {
                entry.arguments = Some(args.clone());
            }
            entry.description = sup.description.clone();
            entry.summary = summary;
            entry.source = supplements.source.clone();
        }
    }

    Ok(by_name.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm_manual::ManualPseudo;
    use crate::llvm::LlvmInstruction;
    use riscv_asm_lsp::data::{InstructionVariant, Xlen};

    #[test]
    fn join_instructions_formats_manual_pseudo_expands_to_once() {
        let llvm = LlvmData {
            extensions: Vec::new(),
            instructions: vec![LlvmInstruction {
                mnemonic: "li".to_owned(),
                kind: InstructionKind::Pseudo,
                variant: InstructionVariant {
                    syntax: "li rd, imm".to_owned(),
                    operands: Vec::new(),
                    required_extensions: vec!["I".to_owned()],
                    xlen: Xlen::Both,
                    alias_of: None,
                    encoding: None,
                    alias_expansion: None,
                },
            }],
            csrs: Vec::new(),
            registers: Vec::new(),
        };
        let udb = UdbData {
            instructions: Vec::new(),
            csrs: Vec::new(),
            registers: HashMap::new(),
            cleaned_todos: Vec::new(),
            copyrights: Vec::new(),
        };
        let manual = AsmManualData {
            pseudos: vec![ManualPseudo {
                mnemonic: "li".to_owned(),
                syntax: "li rd, imm".to_owned(),
                expansion: "Multiple instruction sequences".to_owned(),
                description: "Load immediate".to_owned(),
            }],
            directives: Vec::new(),
        };

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let supplements = load_supplements(root).expect("load server/data/supplements.json");
        let mut corrections = Vec::new();
        let (insts, _) = join_instructions(&llvm, &udb, &manual, &supplements, &mut corrections);
        let li = insts.iter().find(|i| i.mnemonic == "li").expect("li entry");
        let desc = li.description.as_deref().expect("li description");
        assert_eq!(desc.matches("expands to").count(), 1, "desc: {desc}");
        assert!(
            !desc.contains("Multiple instruction sequences"),
            "desc: {desc}"
        );
        assert!(
            desc.contains("one or more instructions depending on the value"),
            "desc: {desc}"
        );
        assert!(!desc.contains("|==="));
        assert!(!desc.contains("latexmath:"));
        assert!(!desc.contains("xref:"));
    }

    #[test]
    fn join_applies_supplements_deprecations_csrs_and_typo_fixes() {
        let llvm = LlvmData {
            extensions: Vec::new(),
            instructions: vec![
                LlvmInstruction {
                    mnemonic: "wfi".to_owned(),
                    kind: InstructionKind::Instruction,
                    variant: InstructionVariant {
                        syntax: "wfi".to_owned(),
                        operands: Vec::new(),
                        required_extensions: vec!["I".to_owned()],
                        xlen: Xlen::Both,
                        alias_of: None,
                        encoding: None,
                        alias_expansion: None,
                    },
                },
                LlvmInstruction {
                    mnemonic: "frsr".to_owned(),
                    kind: InstructionKind::Alias,
                    variant: InstructionVariant {
                        syntax: "frsr rd".to_owned(),
                        operands: Vec::new(),
                        required_extensions: vec!["F".to_owned()],
                        xlen: Xlen::Both,
                        alias_of: Some("csrrs".to_owned()),
                        encoding: None,
                        alias_expansion: None,
                    },
                },
            ],
            csrs: vec![crate::llvm::LlvmCsr {
                name: "stopi".to_owned(),
                number: 0xdb0,
                is_rv32_only: false,
                alt_name: None,
                deprecated_name: None,
            }],
            registers: Vec::new(),
        };
        let udb_insts = vec![crate::udb::UdbInstruction {
            name: "wfi".to_owned(),
            long_name: Some("Wait for interrupt".to_owned()),
            description: Some("Can causes the processor to enter a low-power state.".to_owned()),
            assembly: None,
            extensions: vec!["I".to_owned()],
            xlen: Xlen::Both,
            alias_of: None,
            is_pseudo: false,
        }];
        let udb = UdbData {
            instructions: udb_insts,
            csrs: Vec::new(),
            registers: HashMap::new(),
            cleaned_todos: Vec::new(),
            copyrights: Vec::new(),
        };
        let manual = AsmManualData {
            pseudos: Vec::new(),
            directives: Vec::new(),
        };

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let supplements = load_supplements(root).expect("load server/data/supplements.json");
        let mut corrections = Vec::new();
        let (insts, inst_report) =
            join_instructions(&llvm, &udb, &manual, &supplements, &mut corrections);
        let wfi = insts.iter().find(|i| i.mnemonic == "wfi").expect("wfi");
        assert_eq!(
            wfi.description.as_deref(),
            Some("Can cause the processor to enter a low-power state.")
        );
        assert_eq!(corrections.len(), 1);

        let frsr = insts.iter().find(|i| i.mnemonic == "frsr").expect("frsr");
        assert!(frsr.deprecated);
        assert_eq!(frsr.preferred_name.as_deref(), Some("frcsr"));
        assert!(frsr.description.is_some());
        assert_eq!(
            frsr.description_source.as_deref(),
            Some("riscv-asm-lsp (MIT)")
        );
        assert_eq!(inst_report.llvm_only_with_supplement, vec!["frsr"]);
        assert!(inst_report.llvm_only_no_desc.is_empty());

        let (csrs, csr_report) = join_csrs(&llvm, &udb, &supplements, &mut corrections);
        let stopi = csrs.iter().find(|c| c.name == "stopi").expect("stopi");
        assert_eq!(stopi.privilege.as_deref(), Some("S"));
        assert!(stopi.description.is_some());
        assert_eq!(
            stopi.description_source.as_deref(),
            Some("riscv-asm-lsp (MIT)")
        );
        assert_eq!(csr_report.llvm_only_with_supplement.len(), 1);
    }

    #[test]
    fn make_summary_joins_colon_bullet_lists_and_skips_spec_table_placeholders() {
        let amo = "Atomically with acquire ordering:\n\n  * Load the word at address _xs1_\n  * Write the sign-extended value into _xd_\n  * Add _xs2_ to the loaded value\n  * Write the result to the address in _xs1_";
        let sum = make_summary(amo, SUMMARY_ENTRY_MAX_CHARS).expect("amo summary");
        assert!(!sum.trim_end().ends_with(':'), "ended with colon: {sum}");
        assert!(
            sum.starts_with("Atomically with acquire ordering: load the word"),
            "unexpected amo summary: {sum}"
        );
        assert!(sum.ends_with('.'));

        let sctrclr = "When `mstateen0.CTR`=1, the SCTRCLR instruction performs:\n\n* Zeroes all CTR entries\n** `ctrdata.CC` and `ctrdata.CCV` bit fields.\n\nOther details.";
        let s_sum = make_summary(sctrclr, SUMMARY_ENTRY_MAX_CHARS).expect("sctrclr summary");
        assert!(!s_sum.trim_end().ends_with(':'), "{s_sum}");
        assert!(s_sum.contains("zeroes all CTR entries"), "{s_sum}");

        let frm = "**Floating-Point Dynamic Rounding Mode**\n\nRounding modes are encoded as follows:\n\n*See the RISC-V ISA specification for the full table.*\n\nThe behavior when executed with a reserved rounding mode is _reserved_.";
        let frm_sum = make_summary(frm, SUMMARY_ENTRY_MAX_CHARS).expect("frm summary");
        assert!(!frm_sum.trim_end().ends_with(':'), "{frm_sum}");
        assert!(frm_sum.contains("_reserved_"), "{frm_sum}");
    }

    #[test]
    fn directives_apply_supplements_without_dangling_crossrefs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let supplements = load_supplements(root).expect("load server/data/supplements.json");
        let manual = AsmManualData {
            pseudos: Vec::new(),
            directives: vec![
                DirectiveEntry {
                    name: ".option".to_owned(),
                    arguments: Some("{arch,rvc,norvc,pic,nopic,relax,norelax,push,pop}".to_owned()),
                    summary: None,
                    description:
                        "RISC-V options. Refer to .option for a more detailed description."
                            .to_owned(),
                    source: "riscv-asm-manual (CC-BY-4.0)".to_owned(),
                    llvm_supported: true,
                },
                DirectiveEntry {
                    name: ".globl".to_owned(),
                    arguments: Some("symbol_name".to_owned()),
                    summary: None,
                    description: "emit symbol_name to symbol table (scope GLOBAL)".to_owned(),
                    source: "riscv-asm-manual (CC-BY-4.0)".to_owned(),
                    llvm_supported: true,
                },
            ],
        };
        let dirs = join_directives(root, &manual, &supplements).expect("join directives");
        assert!(!dirs.is_empty());
        for d in &dirs {
            let text = d.summary.as_deref().unwrap_or(&d.description);
            assert!(
                !text.trim_end().ends_with(':'),
                "{} summary ends with colon: {text}",
                d.name
            );
            assert!(
                !text.to_ascii_lowercase().contains("see .") && !text.contains("Refer to"),
                "{} contains dangling cross-reference: {text}",
                d.name
            );
        }
        let option = dirs.iter().find(|d| d.name == ".option").expect(".option");
        assert!(option.arguments.is_some());
        assert!(option.description.contains("arch"));
        assert_eq!(option.source, "riscv-asm-lsp (MIT)");
    }
}
