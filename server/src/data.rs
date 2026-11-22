use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataMeta {
    pub generated: bool,
    pub llvm_tag: String,
    pub udb_commit: String,
    pub asm_manual_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionEntry {
    pub march_name: String,
    pub llvm_feature: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionKind {
    Instruction,
    Alias,
    Pseudo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Xlen {
    #[default]
    Both,
    Rv32,
    Rv64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperandSpec {
    pub name: String,
    pub class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionVariant {
    pub syntax: String,
    pub operands: Vec<OperandSpec>,
    pub required_extensions: Vec<String>,
    pub xlen: Xlen,
    pub alias_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionEntry {
    pub mnemonic: String,
    pub kind: InstructionKind,
    pub variants: Vec<InstructionVariant>,
    #[serde(default)]
    pub udb_assembly: Option<String>,
    pub long_name: Option<String>,
    pub description: Option<String>,
    pub description_source: Option<String>,
    pub llvm_supported: bool,
    pub deprecated: bool,
    pub preferred_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CsrField {
    pub name: String,
    pub bits: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CsrEntry {
    pub name: String,
    pub number: u16,
    pub privilege: Option<String>,
    pub required_extensions: Vec<String>,
    pub fields: Vec<CsrField>,
    pub description: Option<String>,
    #[serde(default)]
    pub description_source: Option<String>,
    pub deprecated_names: Vec<String>,
    #[serde(default)]
    pub xlen: Xlen,
    pub llvm_supported: bool,
}

impl CsrEntry {
    pub fn privilege_label(&self) -> Option<&str> {
        self.privilege.as_deref().map(|p| match p {
            "M" => "M-mode",
            "S" => "S-mode",
            "U" => "U-mode",
            "VS" => "VS-mode",
            "HS" => "HS-mode",
            "VS/HS" => "VS/HS-mode",
            "D" => "Debug",
            other => other,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegisterClass {
    Integer,
    Float,
    Vector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterEntry {
    pub name: String,
    pub number: u8,
    pub abi_names: Vec<String>,
    pub class: RegisterClass,
    pub description: Option<String>,
    pub saver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectiveEntry {
    pub name: String,
    pub arguments: Option<String>,
    pub description: String,
    pub source: String,
    pub llvm_supported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RiscvData {
    pub meta: DataMeta,
    pub extensions: Vec<ExtensionEntry>,
    pub instructions: Vec<InstructionEntry>,
    pub csrs: Vec<CsrEntry>,
    pub registers: Vec<RegisterEntry>,
    pub directives: Vec<DirectiveEntry>,
}

#[derive(Debug)]
pub struct Database {
    pub raw: RiscvData,
    instructions_by_mnemonic: HashMap<String, usize>,
    csrs_by_name: HashMap<String, usize>,
    csrs_by_number: HashMap<u16, usize>,
    registers_by_name: HashMap<String, usize>,
    directives_by_name: HashMap<String, usize>,
    extensions_by_march: HashMap<String, usize>,
}

const EMBEDDED_DATA_JSON: &str = include_str!("../../data/riscv.json");

impl Database {
    /// Load the embedded RISC-V instruction database.
    pub fn load() -> Result<Self, serde_json::Error> {
        let raw: RiscvData = serde_json::from_str(EMBEDDED_DATA_JSON)?;
        Ok(Self::from_data(raw))
    }

    /// Build a lookup database from parsed `RiscvData`.
    pub fn from_data(raw: RiscvData) -> Self {
        let mut instructions_by_mnemonic = HashMap::with_capacity(raw.instructions.len());
        for (idx, inst) in raw.instructions.iter().enumerate() {
            instructions_by_mnemonic
                .entry(inst.mnemonic.to_ascii_lowercase())
                .or_insert(idx);
        }

        let mut csrs_by_name = HashMap::with_capacity(raw.csrs.len() * 2);
        let mut csrs_by_number: HashMap<u16, usize> = HashMap::with_capacity(raw.csrs.len());
        for (idx, csr) in raw.csrs.iter().enumerate() {
            csrs_by_name
                .entry(csr.name.to_ascii_lowercase())
                .or_insert(idx);
            for dep in &csr.deprecated_names {
                csrs_by_name.entry(dep.to_ascii_lowercase()).or_insert(idx);
            }
            csrs_by_number
                .entry(csr.number)
                .and_modify(|existing_idx| {
                    if !raw.csrs[*existing_idx].llvm_supported && csr.llvm_supported {
                        *existing_idx = idx;
                    }
                })
                .or_insert(idx);
        }

        let mut registers_by_name = HashMap::with_capacity(raw.registers.len() * 3);
        for (idx, reg) in raw.registers.iter().enumerate() {
            registers_by_name
                .entry(reg.name.to_ascii_lowercase())
                .or_insert(idx);
            for abi in &reg.abi_names {
                registers_by_name
                    .entry(abi.to_ascii_lowercase())
                    .or_insert(idx);
            }
        }

        let mut directives_by_name = HashMap::with_capacity(raw.directives.len());
        for (idx, dir) in raw.directives.iter().enumerate() {
            directives_by_name
                .entry(dir.name.to_ascii_lowercase())
                .or_insert(idx);
        }

        let mut extensions_by_march = HashMap::with_capacity(raw.extensions.len());
        for (idx, ext) in raw.extensions.iter().enumerate() {
            extensions_by_march
                .entry(ext.march_name.to_ascii_lowercase())
                .or_insert(idx);
        }

        Self {
            raw,
            instructions_by_mnemonic,
            csrs_by_name,
            csrs_by_number,
            registers_by_name,
            directives_by_name,
            extensions_by_march,
        }
    }

    pub fn instruction(&self, mnemonic: &str) -> Option<&InstructionEntry> {
        let idx = self
            .instructions_by_mnemonic
            .get(&mnemonic.to_ascii_lowercase())?;
        self.raw.instructions.get(*idx)
    }

    pub fn csr(&self, name: &str) -> Option<&CsrEntry> {
        let idx = self.csrs_by_name.get(&name.to_ascii_lowercase())?;
        self.raw.csrs.get(*idx)
    }

    pub fn csr_by_number(&self, number: u16) -> Option<&CsrEntry> {
        let idx = self.csrs_by_number.get(&number)?;
        self.raw.csrs.get(*idx)
    }

    pub fn register(&self, name: &str) -> Option<&RegisterEntry> {
        let idx = self.registers_by_name.get(&name.to_ascii_lowercase())?;
        self.raw.registers.get(*idx)
    }

    pub fn directive(&self, name: &str) -> Option<&DirectiveEntry> {
        let key = if name.starts_with('.') {
            name.to_ascii_lowercase()
        } else {
            format!(".{}", name.to_ascii_lowercase())
        };
        let idx = self.directives_by_name.get(&key)?;
        self.raw.directives.get(*idx)
    }

    pub fn extension(&self, march_name: &str) -> Option<&ExtensionEntry> {
        let idx = self
            .extensions_by_march
            .get(&march_name.to_ascii_lowercase())?;
        self.raw.extensions.get(*idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_embedded_database_without_panicking() {
        let db = Database::load();
        assert!(db.is_ok(), "embedded riscv.json must parse: {:?}", db.err());
    }

    #[test]
    fn no_instruction_mnemonic_starts_with_dot() {
        let db = Database::load().expect("db");
        let dot_mnemonics: Vec<&str> = db
            .raw
            .instructions
            .iter()
            .filter(|i| i.mnemonic.starts_with('.'))
            .map(|i| i.mnemonic.as_str())
            .collect();
        assert!(
            dot_mnemonics.is_empty(),
            "unexpected dot-prefixed mnemonics in instructions: {dot_mnemonics:?}"
        );
    }

    #[test]
    fn descriptions_have_no_uncleaned_asciidoc_or_duplicate_expands() {
        let db = Database::load().expect("db");
        for inst in &db.raw.instructions {
            if let Some(desc) = &inst.description {
                assert_clean_description(&inst.mnemonic, desc);
            }
        }
        for csr in &db.raw.csrs {
            if let Some(desc) = &csr.description {
                assert_clean_description(&csr.name, desc);
                assert!(
                    csr.description_source.is_some(),
                    "CSR `{}` has a description but no description_source",
                    csr.name
                );
            }
            for f in &csr.fields {
                if let Some(desc) = &f.description {
                    assert_clean_description(&format!("{}.{}", csr.name, f.name), desc);
                }
            }
        }
        assert_eq!(
            db.csr("stvec")
                .and_then(|c| c.description_source.as_deref()),
            Some("riscv-unified-db (BSD-3-Clause-Clear)")
        );
        assert_eq!(
            db.csr("stopi")
                .and_then(|c| c.description_source.as_deref()),
            Some("riscv-asm-lsp (MIT)")
        );
    }

    fn assert_clean_description(owner: &str, desc: &str) {
        for forbidden in ["|===", "!===", "latexmath:", "xref:", "{fn-", "*Myriad"] {
            assert!(
                !desc.contains(forbidden),
                "{owner} description still contains `{forbidden}`: {desc}"
            );
        }
        assert!(
            desc.matches("expands to").count() <= 1,
            "{owner} description repeats `expands to`: {desc}"
        );
        for line in desc.lines() {
            let t = line.trim();
            assert!(
                !(t.starts_with("[[") && t.ends_with("]]")),
                "{owner} description contains standalone AsciiDoc anchor line `{t}`"
            );
        }
    }
}
