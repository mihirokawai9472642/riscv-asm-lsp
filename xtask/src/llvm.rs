use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::process::Command;

use riscv_asm_lsp::data::{
    ExtensionEntry, InstructionKind, InstructionVariant, OperandSpec, RegisterClass, RegisterEntry,
    Xlen,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct LlvmInstruction {
    pub mnemonic: String,
    pub kind: InstructionKind,
    pub variant: InstructionVariant,
}

#[derive(Debug, Clone)]
pub struct LlvmCsr {
    pub name: String,
    pub alt_name: Option<String>,
    pub deprecated_name: Option<String>,
    pub number: u16,
    pub is_rv32_only: bool,
}

#[derive(Debug, Clone)]
pub struct LlvmData {
    pub extensions: Vec<ExtensionEntry>,
    pub instructions: Vec<LlvmInstruction>,
    pub csrs: Vec<LlvmCsr>,
    pub registers: Vec<RegisterEntry>,
}

pub fn extract(llvm_dir: &Path, cache_dir: &Path) -> Result<LlvmData, Box<dyn std::error::Error>> {
    let json_path = cache_dir.join("riscv-tblgen.json");
    if !json_path.exists() {
        eprintln!("xtask: running llvm-tblgen --dump-json...");
        let include_top = llvm_dir.join("llvm/include");
        let include_target = llvm_dir.join("llvm/lib/Target/RISCV");
        let td_entry = include_target.join("RISCV.td");
        let status = Command::new("llvm-tblgen")
            .arg("--dump-json")
            .arg("-I")
            .arg(&include_top)
            .arg("-I")
            .arg(&include_target)
            .arg(&td_entry)
            .arg("-o")
            .arg(&json_path)
            .status()?;
        if !status.success() {
            return Err(format!("llvm-tblgen exited with {status}").into());
        }
    }

    eprintln!("xtask: parsing TableGen JSON...");
    let file = fs::File::open(&json_path)?;
    let root: Value = serde_json::from_reader(std::io::BufReader::new(file))?;
    let root_obj = root
        .as_object()
        .ok_or("TableGen JSON root is not an object")?;
    let instanceof = root_obj
        .get("!instanceof")
        .and_then(Value::as_object)
        .ok_or("TableGen JSON missing !instanceof")?;

    let clang_exts = query_clang_extensions()?;
    let extensions = extract_extensions(root_obj, instanceof, &clang_exts);
    let instructions = extract_instructions_and_aliases(root_obj, instanceof);
    let csrs = extract_csrs(root_obj, instanceof);
    let registers = extract_registers(root_obj, instanceof);

    Ok(LlvmData {
        extensions,
        instructions,
        csrs,
        registers,
    })
}

fn query_clang_extensions() -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let out = Command::new("clang")
        .args(["--target=riscv64", "--print-supported-extensions"])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("All available")
            || trimmed.starts_with("Name ")
            || trimmed.starts_with("Experimental")
            || trimmed.starts_with("Supported")
            || trimmed.starts_with("Use ")
            || trimmed.starts_with("For ")
            || trimmed.starts_with("Debian ")
            || trimmed.starts_with("clang ")
            || trimmed.starts_with("Target:")
            || trimmed.starts_with("Thread ")
            || trimmed.starts_with("InstalledDir:")
        {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(ver) = parts.next() else { continue };
        if !ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if name.starts_with('x') {
            // Skip vendor extensions per Design 4.3
            continue;
        }
        let rest_start = trimmed
            .find(ver)
            .map(|i| i + ver.len())
            .unwrap_or(trimmed.len());
        let desc = trimmed[rest_start..].trim().to_owned();
        map.insert(name.to_ascii_lowercase(), desc);
    }
    Ok(map)
}

fn extract_extensions(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
    clang_exts: &HashMap<String, String>,
) -> Vec<ExtensionEntry> {
    let mut by_march: BTreeMap<String, ExtensionEntry> = BTreeMap::new();
    if let Some(names) = instanceof.get("SubtargetFeature").and_then(Value::as_array) {
        for def_name_val in names {
            let Some(def_name) = def_name_val.as_str() else {
                continue;
            };
            let Some(rec) = root.get(def_name).and_then(Value::as_object) else {
                continue;
            };
            let superclasses: Vec<&str> = rec
                .get("!superclasses")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !superclasses.contains(&"RISCVExtension") {
                continue;
            }
            let Some(march_name) = rec.get("Name").and_then(Value::as_str) else {
                continue;
            };
            let lower = march_name.to_ascii_lowercase();
            if lower.starts_with('x') || def_name.contains("Vendor") {
                continue;
            }
            let tblgen_desc = rec
                .get("Desc")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let desc = clang_exts
                .get(&lower)
                .filter(|s| !s.is_empty())
                .cloned()
                .or(tblgen_desc);
            by_march.entry(lower.clone()).or_insert(ExtensionEntry {
                march_name: lower,
                llvm_feature: def_name.to_owned(),
                description: desc,
            });
        }
    }

    // Ensure clang-reported standard extensions are all represented
    for (name, desc) in clang_exts {
        by_march.entry(name.clone()).or_insert(ExtensionEntry {
            march_name: name.clone(),
            llvm_feature: String::new(),
            description: if desc.is_empty() {
                None
            } else {
                Some(desc.clone())
            },
        });
    }

    by_march.into_values().collect()
}

fn extract_instructions_and_aliases(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
) -> Vec<LlvmInstruction> {
    let mut out = Vec::new();
    let mut inst_mnemonic_by_def: HashMap<String, String> = HashMap::new();
    extract_instructions(root, instanceof, &mut inst_mnemonic_by_def, &mut out);
    extract_aliases(root, instanceof, &inst_mnemonic_by_def, &mut out);
    out
}

fn extract_instructions(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
    inst_mnemonic_by_def: &mut HashMap<String, String>,
    out: &mut Vec<LlvmInstruction>,
) {
    let Some(inst_defs) = instanceof.get("Instruction").and_then(Value::as_array) else {
        return;
    };
    for def_val in inst_defs {
        let Some(def_name) = def_val.as_str() else {
            continue;
        };
        let Some(rec) = root.get(def_name).and_then(Value::as_object) else {
            continue;
        };
        if rec.get("Namespace").and_then(Value::as_str) != Some("RISCV") {
            continue;
        }
        if rec
            .get("isCodeGenOnly")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            != 0
        {
            continue;
        }
        let asm_string = rec
            .get("AsmString")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if asm_string.is_empty() {
            continue;
        }
        let pred_names = predicate_def_names(rec);
        if pred_names.iter().any(|p| p.contains("Vendor")) {
            continue;
        }

        let mnemonic = extract_mnemonic(asm_string);
        if mnemonic.is_empty() || mnemonic.starts_with('.') {
            continue;
        }
        inst_mnemonic_by_def.insert(def_name.to_owned(), mnemonic.clone());

        let operand_map = collect_instruction_operand_classes(rec);
        let (syntax, operands) = format_syntax_and_operands(asm_string, &operand_map);
        let (required_extensions, xlen) = decode_predicates(root, &pred_names);
        let kind = if rec.get("isPseudo").and_then(Value::as_i64).unwrap_or(0) != 0 {
            InstructionKind::Pseudo
        } else {
            InstructionKind::Instruction
        };

        out.push(LlvmInstruction {
            mnemonic,
            kind,
            variant: InstructionVariant {
                syntax,
                operands,
                required_extensions,
                xlen,
                alias_of: None,
            },
        });
    }
}

fn extract_aliases(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
    inst_mnemonic_by_def: &HashMap<String, String>,
    out: &mut Vec<LlvmInstruction>,
) {
    let Some(alias_defs) = instanceof.get("InstAlias").and_then(Value::as_array) else {
        return;
    };
    for def_val in alias_defs {
        let Some(def_name) = def_val.as_str() else {
            continue;
        };
        let Some(rec) = root.get(def_name).and_then(Value::as_object) else {
            continue;
        };
        let asm_string = rec
            .get("AsmString")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if asm_string.is_empty() {
            continue;
        }
        let pred_names = predicate_def_names(rec);
        if pred_names.iter().any(|p| p.contains("Vendor")) {
            continue;
        }
        let result_inst = rec.get("ResultInst").and_then(Value::as_object);
        let target_def = result_inst
            .and_then(|r| r.get("operator"))
            .and_then(|o| o.get("def"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(target_rec) = root.get(target_def).and_then(Value::as_object)
            && target_rec.get("Namespace").and_then(Value::as_str) != Some("RISCV")
        {
            continue;
        }
        let mnemonic = extract_mnemonic(asm_string);
        if mnemonic.is_empty() || mnemonic.starts_with('.') {
            continue;
        }

        let operand_map = collect_alias_operand_classes(result_inst);
        let (syntax, operands) = format_syntax_and_operands(asm_string, &operand_map);
        let (required_extensions, xlen) = decode_predicates(root, &pred_names);
        let alias_of = inst_mnemonic_by_def.get(target_def).cloned().or_else(|| {
            root.get(target_def)
                .and_then(|v| v.get("AsmString"))
                .and_then(Value::as_str)
                .map(extract_mnemonic)
                .filter(|s| !s.is_empty())
        });

        out.push(LlvmInstruction {
            mnemonic,
            kind: InstructionKind::Alias,
            variant: InstructionVariant {
                syntax,
                operands,
                required_extensions,
                xlen,
                alias_of,
            },
        });
    }
}

fn predicate_def_names(rec: &serde_json::Map<String, Value>) -> Vec<String> {
    rec.get("Predicates")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("def").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn decode_predicates(
    root: &serde_json::Map<String, Value>,
    pred_names: &[String],
) -> (Vec<String>, Xlen) {
    let mut is_rv32 = false;
    let mut is_rv64 = false;
    let mut reqs = BTreeSet::new();

    for name in pred_names {
        if name == "IsRV32" {
            is_rv32 = true;
            continue;
        }
        if name == "IsRV64" {
            is_rv64 = true;
            continue;
        }
        if let Some(cond) = root
            .get(name)
            .and_then(|v| v.get("CondString"))
            .and_then(Value::as_str)
        {
            for ext_expr in cond_to_ext_reqs(cond) {
                reqs.insert(ext_expr);
            }
        }
    }

    let xlen = match (is_rv32, is_rv64) {
        (true, false) => Xlen::Rv32,
        (false, true) => Xlen::Rv64,
        _ => Xlen::Both,
    };
    (reqs.into_iter().collect(), xlen)
}

fn cond_to_ext_reqs(cond: &str) -> Vec<String> {
    let mut out = Vec::new();
    for clause in cond.split("&&") {
        if let Some(expr) = clause_to_ext_expr(clause)
            && !out.contains(&expr)
        {
            out.push(expr);
        }
    }
    out
}

fn clause_to_ext_expr(clause: &str) -> Option<String> {
    let mut alts = Vec::new();
    let mut rest = clause;
    while let Some(idx) = rest.find("Subtarget->") {
        let prefix = &rest[..idx];
        let is_negated = prefix.trim_end().ends_with('!');
        let after = &rest[idx + "Subtarget->".len()..];
        let end = after.find(')').map(|p| p + 1).unwrap_or(after.len());
        let call = &after[..end];
        rest = &after[end..];
        if is_negated {
            continue;
        }
        if let Some(ext) = call
            .strip_prefix("hasStdExt")
            .and_then(|s| s.strip_suffix("()"))
        {
            for seg in ext.split("Or") {
                alts.push(seg.to_ascii_lowercase());
            }
        } else if let Some(vec_expr) = map_v_predicate(call) {
            for part in vec_expr.split('|') {
                alts.push(part.to_owned());
            }
        } else if call.starts_with("hasHalfFPLoadStoreMove") {
            alts.push("zfh".to_owned());
            alts.push("zfhmin".to_owned());
            alts.push("zfbfmin".to_owned());
        } else if call.starts_with("enableRVCHintInstrs") {
            alts.push("c".to_owned());
            alts.push("zca".to_owned());
        }
    }
    if alts.is_empty() {
        None
    } else {
        let mut seen = BTreeSet::new();
        let deduped: Vec<String> = alts
            .into_iter()
            .filter(|a| seen.insert(a.clone()))
            .collect();
        Some(deduped.join("|"))
    }
}

fn map_v_predicate(call: &str) -> Option<&'static str> {
    match call {
        "hasVInstructions()" | "hasVInstructionsFullMultiply()" => Some("v|zve32x"),
        "hasVInstructionsI64()" => Some("v|zve64x"),
        "hasVInstructionsAnyF()" => Some("v|zve32f"),
        "hasVInstructionsF64()" => Some("v|zve64d"),
        "hasVInstructionsF16()" => Some("zvfh"),
        "hasVInstructionsF16Minimal()" => Some("zvfh|zvfhmin"),
        "hasVInstructionsBF16()" => Some("zvfbfmin"),
        _ if call.starts_with("hasVInstructions") => Some("v|zve32x"),
        _ => None,
    }
}

fn extract_mnemonic(asm_string: &str) -> String {
    let trimmed = asm_string.trim();
    trimmed
        .split([' ', '\t'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn collect_instruction_operand_classes(
    rec: &serde_json::Map<String, Value>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for list_key in ["OutOperandList", "InOperandList"] {
        if let Some(args) = rec
            .get(list_key)
            .and_then(|v| v.get("args"))
            .and_then(Value::as_array)
        {
            collect_dag_args(args, &mut map);
        }
    }
    map
}

fn collect_alias_operand_classes(
    result_inst: Option<&serde_json::Map<String, Value>>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(args) = result_inst
        .and_then(|r| r.get("args"))
        .and_then(Value::as_array)
    {
        collect_dag_args(args, &mut map);
    }
    map
}

fn collect_dag_args(args: &[Value], map: &mut HashMap<String, String>) {
    for arg in args {
        let Some(pair) = arg.as_array() else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let Some(var_name) = pair[1].as_str() else {
            continue;
        };
        if let Some(def_name) = pair[0].get("def").and_then(Value::as_str) {
            map.entry(var_name.to_owned())
                .or_insert_with(|| def_name.to_owned());
        }
    }
}

fn format_syntax_and_operands(
    asm_string: &str,
    operand_classes: &HashMap<String, String>,
) -> (String, Vec<OperandSpec>) {
    let mut syntax = String::with_capacity(asm_string.len());
    let mut operands = Vec::new();
    let mut seen_vars = BTreeSet::new();

    let bytes = asm_string.as_bytes();
    let mut i = 0;
    let mut just_emitted_var = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\t' {
            syntax.push(' ');
            just_emitted_var = false;
            i += 1;
            continue;
        }
        if b == b'$' {
            let var_opt = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                asm_string[i + 2..]
                    .find('}')
                    .map(|close| (&asm_string[i + 2..i + 2 + close], i + 2 + close + 1))
            } else {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                (end > start).then(|| (&asm_string[start..end], end))
            };

            if let Some((var, next_i)) = var_opt {
                let class = operand_classes
                    .get(var)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned());
                if just_emitted_var {
                    syntax.push_str(", ");
                }
                let needs_parens = class == "GPRMemZeroOffset"
                    && !syntax.ends_with('(')
                    && bytes.get(next_i) != Some(&b')');
                if needs_parens {
                    syntax.push('(');
                    syntax.push_str(var);
                    syntax.push(')');
                } else {
                    syntax.push_str(var);
                }
                if seen_vars.insert(var.to_owned()) {
                    operands.push(OperandSpec {
                        name: var.to_owned(),
                        class,
                    });
                }
                just_emitted_var = true;
                i = next_i;
                continue;
            }
        }
        if let Some(ch) = asm_string[i..].chars().next() {
            syntax.push(ch);
            just_emitted_var = false;
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    let normalized = syntax.split_whitespace().collect::<Vec<_>>().join(" ");
    (normalized, operands)
}

fn extract_csrs(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
) -> Vec<LlvmCsr> {
    let mut out = Vec::new();
    let Some(names) = instanceof.get("SysReg").and_then(Value::as_array) else {
        return out;
    };
    for def_val in names {
        let Some(def_name) = def_val.as_str() else {
            continue;
        };
        let Some(rec) = root.get(def_name).and_then(Value::as_object) else {
            continue;
        };
        let Some(name) = rec.get("Name").and_then(Value::as_str) else {
            continue;
        };
        let Some(enc_bits) = rec.get("Encoding").and_then(Value::as_array) else {
            continue;
        };
        let mut number: u16 = 0;
        for (bit_idx, bit_val) in enc_bits.iter().enumerate() {
            if bit_val.as_i64() == Some(1) && bit_idx < 16 {
                number |= 1 << bit_idx;
            }
        }
        let alt_name = rec
            .get("AltName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(name))
            .map(|s| s.to_ascii_lowercase());
        let deprecated_name = rec
            .get("DeprecatedName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());
        let is_rv32_only = rec.get("isRV32Only").and_then(Value::as_i64).unwrap_or(0) != 0;

        out.push(LlvmCsr {
            name: name.to_ascii_lowercase(),
            alt_name,
            deprecated_name,
            number,
            is_rv32_only,
        });
    }
    out
}

fn extract_registers(
    root: &serde_json::Map<String, Value>,
    instanceof: &serde_json::Map<String, Value>,
) -> Vec<RegisterEntry> {
    let mut by_name: BTreeMap<(u8, u8, String), RegisterEntry> = BTreeMap::new();
    let Some(names) = instanceof.get("Register").and_then(Value::as_array) else {
        return Vec::new();
    };

    for def_val in names {
        let Some(def_name) = def_val.as_str() else {
            continue;
        };
        let Some(rec) = root.get(def_name).and_then(Value::as_object) else {
            continue;
        };
        if rec.get("Namespace").and_then(Value::as_str) != Some("RISCV") {
            continue;
        }
        if rec.get("isArtificial").and_then(Value::as_i64).unwrap_or(0) != 0 {
            continue;
        }
        let Some(asm_name) = rec.get("AsmName").and_then(Value::as_str) else {
            continue;
        };
        let lower = asm_name.to_ascii_lowercase();
        let (class, class_ord, num) = if let Some(n) = parse_numbered_reg(&lower, 'x') {
            (RegisterClass::Integer, 0, n)
        } else if let Some(n) = parse_numbered_reg(&lower, 'f') {
            (RegisterClass::Float, 1, n)
        } else if let Some(n) = parse_numbered_reg(&lower, 'v') {
            (RegisterClass::Vector, 2, n)
        } else {
            continue;
        };
        if num >= 32 {
            continue;
        }

        let alt_names: Vec<String> = rec
            .get("AltNames")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();

        let key = (class_ord, num, lower.clone());
        let entry = by_name.entry(key).or_insert_with(|| RegisterEntry {
            name: lower,
            number: num,
            abi_names: Vec::new(),
            class,
            summary: None,
            description: None,
            saver: None,
        });
        for alt in alt_names {
            if !entry.abi_names.contains(&alt) {
                entry.abi_names.push(alt);
            }
        }
    }

    by_name.into_values().collect()
}

fn parse_numbered_reg(name: &str, prefix: char) -> Option<u8> {
    let rest = name.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cond_to_ext_reqs_distinguishes_vector_subextensions_and_splits_and() {
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasVInstructionsAnyF()"),
            vec!["v|zve32f".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasVInstructionsI64()"),
            vec!["v|zve64x".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasVInstructionsF64()"),
            vec!["v|zve64d".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasVInstructionsF16Minimal()"),
            vec!["zvfh|zvfhmin".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasVInstructionsBF16()"),
            vec!["zvfbfmin".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasStdExtZcmp() && !Subtarget->hasStdExtC()"),
            vec!["zcmp".to_owned()]
        );
        assert_eq!(
            cond_to_ext_reqs("Subtarget->hasStdExtM() && Subtarget->hasStdExtZba()"),
            vec!["m".to_owned(), "zba".to_owned()]
        );
    }
}
