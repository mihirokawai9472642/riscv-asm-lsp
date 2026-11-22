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
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementsFile {
    source: String,
    instructions: Vec<SupplementalInst>,
    csrs: Vec<SupplementalCsr>,
    description_corrections: Vec<DescriptionCorrection>,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementalInst {
    mnemonic: String,
    description: String,
    deprecated: bool,
    preferred_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SupplementalCsr {
    name: String,
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DescriptionCorrection {
    owner: String,
    wrong: String,
    right: String,
}

fn load_supplements(workspace_root: &Path) -> Result<SupplementsFile, Box<dyn std::error::Error>> {
    let path = workspace_root.join("data/supplements.json");
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
    let directives = join_directives(workspace_root, &manual)?;

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

    let mut udb_by_name: HashMap<String, &crate::udb::UdbInstruction> = HashMap::new();
    for inst in &udb.instructions {
        // Prefer real instruction entries over generated pseudo entries if names collide
        udb_by_name
            .entry(inst.name.clone())
            .and_modify(|existing| {
                if existing.is_pseudo && !inst.is_pseudo {
                    *existing = inst;
                }
            })
            .or_insert(inst);
    }

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

    let mut out = Vec::new();
    let mut both = 0usize;
    let mut llvm_only_with_manual = Vec::new();
    let mut llvm_only_with_supplement = Vec::new();
    let mut llvm_only_no_desc = Vec::new();
    let mut udb_only = Vec::new();
    let mut seen_udb = BTreeSet::new();

    for (mnemonic, group) in &llvm_grouped {
        let mut variants: Vec<InstructionVariant> = Vec::new();
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

        let udb_entry = udb_by_name.get(mnemonic).copied();
        let manual_entry = manual_by_name.get(mnemonic).copied();
        let supplement_entry = supplement_by_name.get(mnemonic.as_str()).copied();

        if udb_entry.is_some() {
            seen_udb.insert(mnemonic.clone());
            both += 1;
        } else if manual_entry.is_some() {
            llvm_only_with_manual.push(mnemonic.clone());
        } else if supplement_entry.is_some() {
            llvm_only_with_supplement.push(mnemonic.clone());
        } else {
            llvm_only_no_desc.push(mnemonic.clone());
        }

        let udb_assembly = udb_entry.and_then(|u| u.assembly.clone());
        let (long_name, description, description_source) = if let Some(u) = udb_entry {
            let desc = u.description.clone().map(|d| {
                apply_description_corrections(
                    mnemonic,
                    d,
                    &supplements.description_corrections,
                    corrections,
                )
            });
            let src = if desc.is_some() {
                Some("riscv-unified-db (BSD-3-Clause-Clear)".to_owned())
            } else {
                None
            };
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
        };

        let (deprecated, preferred_name) = if let Some(supp) = supplement_entry {
            (supp.deprecated, supp.preferred_name.clone())
        } else {
            (false, None)
        };
        out.push(InstructionEntry {
            mnemonic: mnemonic.clone(),
            kind,
            variants,
            udb_assembly,
            long_name,
            description,
            description_source,
            llvm_supported: true,
            deprecated,
            preferred_name,
        });
    }

    // Add entries only present in riscv-unified-db (not accepted by LLVM 19)
    let mut missing_rv64gc = Vec::new();
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
        udb_only.push(mnemonic.clone());

        if !u.is_pseudo
            && u.extensions
                .iter()
                .any(|ext| ext.split('|').any(|part| rv64gc_exts.contains(part)))
        {
            missing_rv64gc.push(mnemonic.clone());
        }

        let kind = if u.is_pseudo {
            InstructionKind::Pseudo
        } else {
            InstructionKind::Instruction
        };
        let syntax = u.assembly.clone().unwrap_or_else(|| mnemonic.clone());
        out.push(InstructionEntry {
            mnemonic,
            kind,
            variants: vec![InstructionVariant {
                syntax,
                operands: Vec::<OperandSpec>::new(),
                required_extensions: u.extensions.clone(),
                xlen: u.xlen,
                alias_of: u.alias_of.clone(),
            }],
            udb_assembly: u.assembly.clone(),
            long_name: u.long_name.clone(),
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

    (
        out,
        InstReport {
            both,
            llvm_only_with_manual,
            llvm_only_with_supplement,
            llvm_only_no_desc,
            udb_only,
            missing_rv64gc,
        },
    )
}

struct CsrReport {
    both: usize,
    llvm_only_with_supplement: Vec<(String, u16)>,
    llvm_only: Vec<(String, u16)>,
    udb_only: Vec<(String, u16)>,
    deprecated_joined: Vec<(String, String)>,
}

fn join_csrs(
    llvm: &LlvmData,
    udb: &UdbData,
    supplements: &SupplementsFile,
    corrections: &mut Vec<(String, String, String)>,
) -> (Vec<CsrEntry>, CsrReport) {
    let mut udb_by_name: HashMap<String, &crate::udb::UdbCsr> = HashMap::new();
    let mut udb_by_addr: HashMap<u16, &crate::udb::UdbCsr> = HashMap::new();
    for csr in &udb.csrs {
        udb_by_name.insert(csr.name.clone(), csr);
        udb_by_addr.entry(csr.address).or_insert(csr);
    }
    let supplement_csrs: HashMap<&str, &str> = supplements
        .csrs
        .iter()
        .map(|c| (c.name.as_str(), c.description.as_str()))
        .collect();

    let mut out = Vec::new();
    let mut matched_udb_names = BTreeSet::new();
    let mut both = 0usize;
    let mut llvm_only_with_supplement = Vec::new();
    let mut llvm_only = Vec::new();
    let mut deprecated_joined = Vec::new();
    let mut llvm_sorted = llvm.csrs.clone();
    llvm_sorted.sort_by(|a, b| a.number.cmp(&b.number).then_with(|| a.name.cmp(&b.name)));

    for l in &llvm_sorted {
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
                        deprecated_joined.push((l.name.clone(), dep.to_owned()));
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
            both += 1;
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
            out.push(CsrEntry {
                name: l.name.clone(),
                number: l.number,
                privilege: u.privilege.clone(),
                required_extensions: u.extensions.clone(),
                fields: u.fields.clone(),
                description,
                description_source,
                deprecated_names,
                xlen,
                llvm_supported: true,
            });
        } else {
            let supp_desc = supplement_csrs
                .get(l.name.as_str())
                .map(|s| (*s).to_owned());
            let description_source = if supp_desc.is_some() {
                llvm_only_with_supplement.push((l.name.clone(), l.number));
                Some(supplements.source.clone())
            } else {
                llvm_only.push((l.name.clone(), l.number));
                None
            };
            out.push(CsrEntry {
                name: l.name.clone(),
                number: l.number,
                privilege: infer_csr_privilege(&l.name, l.number),
                required_extensions: Vec::new(),
                fields: Vec::new(),
                description: supp_desc,
                description_source,
                deprecated_names,
                xlen,
                llvm_supported: true,
            });
        }
    }

    let mut udb_only = Vec::new();
    let mut udb_sorted = udb.csrs.clone();
    udb_sorted.sort_by(|a, b| a.address.cmp(&b.address).then_with(|| a.name.cmp(&b.name)));
    for u in udb_sorted {
        if matched_udb_names.contains(&u.name) {
            continue;
        }
        udb_only.push((u.name.clone(), u.address));
        let description = match (&u.long_name, &u.description) {
            (Some(ln), Some(d)) => Some(format!("**{ln}**\n\n{d}")),
            (Some(ln), None) => Some(ln.clone()),
            (None, Some(d)) => Some(d.clone()),
            (None, None) => None,
        };
        let description_source = description
            .as_ref()
            .map(|_| "riscv-unified-db (BSD-3-Clause-Clear)".to_owned());
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
            fields: u.fields,
            description,
            description_source,
            deprecated_names: Vec::new(),
            xlen,
            llvm_supported: false,
        });
    }

    (
        out,
        CsrReport {
            both,
            llvm_only_with_supplement,
            llvm_only,
            udb_only,
            deprecated_joined,
        },
    )
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
                merged.saver = info.saver.clone();
            }
            merged
        })
        .collect()
}

fn join_directives(
    workspace_root: &Path,
    manual: &AsmManualData,
) -> Result<Vec<DirectiveEntry>, Box<dyn std::error::Error>> {
    let mut by_name: BTreeMap<String, DirectiveEntry> = BTreeMap::new();
    for dir in &manual.directives {
        by_name.insert(dir.name.clone(), dir.clone());
    }

    let hand_path = workspace_root.join("data/directives_gnu.json");
    let hand_text = fs::read_to_string(&hand_path)?;
    let hand: HandGnuDirectivesFile = serde_json::from_str(&hand_text)?;
    for dir in hand.directives {
        let name = dir.name.to_ascii_lowercase();
        by_name.entry(name.clone()).or_insert(DirectiveEntry {
            name,
            arguments: dir.arguments,
            description: dir.description,
            source: hand.source.clone(),
            llvm_supported: true,
        });
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
                },
            }],
            csrs: Vec::new(),
            registers: Vec::new(),
        };
        let udb = UdbData {
            instructions: Vec::new(),
            csrs: Vec::new(),
            registers: HashMap::new(),
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
        let supplements = load_supplements(root).expect("load data/supplements.json");
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
        };
        let manual = AsmManualData {
            pseudos: Vec::new(),
            directives: Vec::new(),
        };

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let supplements = load_supplements(root).expect("load data/supplements.json");
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
}
