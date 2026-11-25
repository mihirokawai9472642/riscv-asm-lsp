use std::collections::HashMap;

use crate::context::MarchSpec;
use crate::data::{Database, InstructionVariant, Xlen};
use crate::line::split_unquoted_statements_stripped;

use super::expand::{ExpandedInstruction, substitute_constants};
use super::matint;

/// Expand and encode a single assembly statement purely in Rust using the bundled database
/// (`RISCVMatInt` for `li`, fixed pseudos, `InstAlias` expansions, RVC auto-compression,
/// and TableGen `EncodingTemplate` bit packing).
pub fn expand_and_encode(
    stmt: &str,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<Vec<ExpandedInstruction>> {
    let substituted = if known_consts.is_empty() {
        stmt.to_owned()
    } else {
        substitute_constants(stmt, known_consts)
    };
    let clean = strip_leading_labels(&substituted).trim();
    if clean.is_empty() || clean.starts_with('.') {
        return None;
    }

    // Handle `;`-separated multiple statements cleanly
    let parts = split_unquoted_statements_stripped(clean);
    let mut out = Vec::new();
    for part in parts {
        let p = strip_leading_labels(&part).trim();
        if p.is_empty() || p.starts_with('.') {
            continue;
        }
        let expanded = expand_single_statement(p, march, known_consts, db)?;
        out.extend(expanded);
    }

    if out.is_empty() { None } else { Some(out) }
}

#[derive(Debug, Clone)]
struct InstCall {
    mnemonic: String,
    operands: Vec<String>,
}

impl InstCall {
    fn format_asm(&self) -> String {
        if self.operands.is_empty() {
            self.mnemonic.clone()
        } else {
            format!("{} {}", self.mnemonic, self.operands.join(", "))
        }
    }
}

fn expand_single_statement(
    stmt: &str,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<Vec<ExpandedInstruction>> {
    let (mnem, ops) = parse_stmt(stmt)?;
    let lower = mnem.to_ascii_lowercase();

    // 1. Fixed pseudo: `li rd, imm` -> `RISCVMatInt`
    if lower == "li" && ops.len() == 2 {
        let val = eval_u64_const_expr(&ops[1], known_consts)? as i64;
        let rd = ops[0].trim();
        db.register(rd)?;
        let seq = matint::materialize_li(rd, val, march);
        return Some(
            seq.into_iter()
                .map(|m| {
                    compress_and_encode(
                        InstCall {
                            mnemonic: m.mnemonic.to_owned(),
                            operands: m.operands,
                        },
                        march,
                        known_consts,
                        db,
                    )
                })
                .collect(),
        );
    }

    // 2. Fixed relocation pseudos: `la`, `lla`, `call`, `tail`, `jump`, symbol load/store
    if let Some(reloc_seq) = try_expand_fixed_pseudo(&lower, &ops, march, known_consts, db) {
        return Some(reloc_seq);
    }

    // 3. Real canonical or compressed instruction before same-mnemonic fallback aliases (e.g. `add rd, rs1, rs2` vs alias `add rd, rs1, imm12`)
    let call = InstCall {
        mnemonic: lower.clone(),
        operands: ops.clone(),
    };
    if matches_active_encoding_variant(&call, march, known_consts, db) {
        return Some(vec![compress_and_encode(call, march, known_consts, db)]);
    }

    // 4. `InstAlias` expansion from `data/riscv.json` (`alias_expansion`)
    if let Some(canon) = try_expand_alias(&lower, &ops, march, known_consts, db) {
        return Some(vec![compress_and_encode(canon, march, known_consts, db)]);
    }

    // 5. Fallback for valid active instruction variants without an encoding template
    let inst = db.instruction(&call.mnemonic)?;
    if !inst
        .variants
        .iter()
        .any(|v| march.supports_extensions(&v.required_extensions, v.xlen))
    {
        return None;
    }

    Some(vec![compress_and_encode(call, march, known_consts, db)])
}

fn try_expand_fixed_pseudo(
    lower: &str,
    ops: &[String],
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<Vec<ExpandedInstruction>> {
    match lower {
        "la" | "lla" if ops.len() == 2 => {
            let rd = ops[0].trim();
            let target = ops[1].trim();
            if lower == "la"
                && let Some(val) = eval_u64_const_expr(target, known_consts)
            {
                let seq = matint::materialize_li(rd, val as i64, march);
                return Some(
                    seq.into_iter()
                        .map(|m| {
                            compress_and_encode(
                                InstCall {
                                    mnemonic: m.mnemonic.to_owned(),
                                    operands: m.operands,
                                },
                                march,
                                known_consts,
                                db,
                            )
                        })
                        .collect(),
                );
            }
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc {rd}, %pcrel_hi({target})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: format!("addi {rd}, {rd}, %pcrel_lo(.Lpcrel_hi0)"),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        "call" if ops.len() == 1 || ops.len() == 2 => {
            let (rd, target) = if ops.len() == 2 {
                (ops[0].trim(), ops[1].trim())
            } else {
                ("ra", ops[0].trim())
            };
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc {rd}, %call_plt({target})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: format!("jalr {rd}, 0({rd})"),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        "tail" if ops.len() == 1 => {
            let target = ops[0].trim();
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc t1, %call_plt({target})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: "jalr zero, 0(t1)".to_owned(),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        "jump" if ops.len() == 2 => {
            let target = ops[0].trim();
            let rt = ops[1].trim();
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc {rt}, %pcrel_hi({target})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: format!("jalr zero, %pcrel_lo(.Lpcrel_hi0)({rt})"),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        "lb" | "lh" | "lw" | "ld" | "lbu" | "lhu" | "lwu" | "flw" | "fld"
            if ops.len() == 2
                && !ops[1].contains('(')
                && eval_u64_const_expr(&ops[1], known_consts).is_none() =>
        {
            let rd = ops[0].trim();
            let sym = ops[1].trim();
            db.register(rd)?;
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc {rd}, %pcrel_hi({sym})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: format!("{lower} {rd}, %pcrel_lo(.Lpcrel_hi0)({rd})"),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        "sb" | "sh" | "sw" | "sd" | "fsw" | "fsd" if ops.len() == 3 && !ops[1].contains('(') => {
            let rs = ops[0].trim();
            let sym = ops[1].trim();
            let rt = ops[2].trim();
            Some(vec![
                ExpandedInstruction {
                    asm: format!("auipc {rt}, %pcrel_hi({sym})"),
                    encoding: Some("(relocated)".to_owned()),
                },
                ExpandedInstruction {
                    asm: format!("{lower} {rs}, %pcrel_lo(.Lpcrel_hi0)({rt})"),
                    encoding: Some("(relocated)".to_owned()),
                },
            ])
        }
        _ => None,
    }
}

fn matches_active_encoding_variant(
    call: &InstCall,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> bool {
    let Some(inst) = db.instruction(&call.mnemonic) else {
        return false;
    };
    inst.variants.iter().any(|v| {
        march.supports_extensions(&v.required_extensions, v.xlen)
            && v.encoding.is_some()
            && bind_typed_variant_operands(v, &call.operands, known_consts, db).is_some()
    })
}

fn try_expand_alias(
    lower: &str,
    user_ops: &[String],
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<InstCall> {
    let inst = db.instruction(lower)?;
    for v in &inst.variants {
        if !march.supports_extensions(&v.required_extensions, v.xlen) {
            continue;
        }
        let Some(ae) = &v.alias_expansion else {
            continue;
        };
        let Some(alias_bindings) = bind_typed_variant_operands(v, user_ops, known_consts, db)
        else {
            continue;
        };

        let mut target_bindings: HashMap<String, String> = HashMap::new();
        for b in &ae.bindings {
            let val = if let Some(var_name) = b.value.strip_prefix('$') {
                alias_bindings.get(var_name)?.clone()
            } else if b.value.starts_with("0x")
                && let Some(op_spec) = db.instruction(&ae.mnemonic).and_then(|ti| {
                    ti.variants
                        .iter()
                        .flat_map(|tv| tv.operands.iter())
                        .find(|o| o.name == b.target_operand)
                })
                && op_spec.class == "csr_sysreg"
                && let Ok(num) = u16::from_str_radix(b.value.trim_start_matches("0x"), 16)
                && let Some(csr) = db.csr_by_number(num)
            {
                csr.name.clone()
            } else {
                b.value.clone()
            };
            target_bindings.insert(b.target_operand.clone(), val);
        }

        let target_ops = render_target_operands(&ae.target_syntax, &target_bindings)?;
        return Some(InstCall {
            mnemonic: ae.mnemonic.clone(),
            operands: target_ops,
        });
    }
    None
}

fn compress_and_encode(
    call: InstCall,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> ExpandedInstruction {
    let final_call = try_compress(&call, march, known_consts, db).unwrap_or(call);
    let encoding = encode_call(&final_call, march, known_consts, db);
    ExpandedInstruction {
        asm: final_call.format_asm(),
        encoding,
    }
}

fn try_compress(
    call: &InstCall,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<InstCall> {
    if !march.extensions.contains("zca") {
        return None;
    }
    let reg_num = |tok: &str| db.register(tok.trim()).map(|r| r.number);
    let f_reg_num = |tok: &str| {
        db.register(tok.trim())
            .filter(|r| r.class == crate::data::RegisterClass::Float)
            .map(|r| r.number)
    };
    let i_reg_num = |tok: &str| {
        db.register(tok.trim())
            .filter(|r| r.class == crate::data::RegisterClass::Integer)
            .map(|r| r.number)
    };
    let is_c_gpr = |n: u8| (8..=15).contains(&n);
    let is_c_fpr = |n: u8| (8..=15).contains(&n);
    let eval_i = |tok: &str| eval_i128_const_expr(tok, known_consts);

    match call.mnemonic.as_str() {
        "addi" if call.operands.len() == 3 => {
            let (rd_s, rs_s, imm_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs = i_reg_num(rs_s)?;
            let imm = eval_i(imm_s)?;
            if rd == 0 && rs == 0 {
                if imm == 0 {
                    return Some(InstCall {
                        mnemonic: "c.nop".to_owned(),
                        operands: Vec::new(),
                    });
                }
                if (-32..=31).contains(&imm) {
                    return Some(InstCall {
                        mnemonic: "c.nop".to_owned(),
                        operands: vec![imm.to_string()],
                    });
                }
                return None;
            }
            if rd != 0 && rs == 0 && (-32..=31).contains(&imm) {
                return Some(InstCall {
                    mnemonic: "c.li".to_owned(),
                    operands: vec![rd_s.clone(), imm.to_string()],
                });
            }
            if imm == 0 && rd != 0 && rs != 0 {
                return Some(InstCall {
                    mnemonic: "c.mv".to_owned(),
                    operands: vec![rd_s.clone(), rs_s.clone()],
                });
            }
            if rd != 0 && rd == rs {
                if (-32..=31).contains(&imm) && imm != 0 {
                    return Some(InstCall {
                        mnemonic: "c.addi".to_owned(),
                        operands: vec![rd_s.clone(), imm.to_string()],
                    });
                }
                if rd == 2 && imm != 0 && (-512..=496).contains(&imm) && imm.rem_euclid(16) == 0 {
                    return Some(InstCall {
                        mnemonic: "c.addi16sp".to_owned(),
                        operands: vec!["sp".to_owned(), imm.to_string()],
                    });
                }
            }
            if rs == 2 && is_c_gpr(rd) && imm > 0 && imm <= 1020 && imm.rem_euclid(4) == 0 {
                return Some(InstCall {
                    mnemonic: "c.addi4spn".to_owned(),
                    operands: vec![rd_s.clone(), "sp".to_owned(), imm.to_string()],
                });
            }
        }
        "addiw" if call.operands.len() == 3 && march.xlen != Xlen::Rv32 => {
            let (rd_s, rs_s, imm_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs = i_reg_num(rs_s)?;
            let imm = eval_i(imm_s)?;
            if rd != 0 && rd == rs && (-32..=31).contains(&imm) {
                return Some(InstCall {
                    mnemonic: "c.addiw".to_owned(),
                    operands: vec![rd_s.clone(), imm.to_string()],
                });
            }
        }
        "add" if call.operands.len() == 3 => {
            let (rd_s, rs1_s, rs2_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs1 = i_reg_num(rs1_s)?;
            let rs2 = i_reg_num(rs2_s)?;
            if rd != 0 && rd == rs1 && rs2 != 0 {
                return Some(InstCall {
                    mnemonic: "c.add".to_owned(),
                    operands: vec![rd_s.clone(), rs2_s.clone()],
                });
            }
        }
        "lui" if call.operands.len() == 2 => {
            let (rd_s, imm_s) = (&call.operands[0], &call.operands[1]);
            let rd = i_reg_num(rd_s)?;
            let imm = eval_i(imm_s)?;
            if rd != 0 && rd != 2 && ((1..=31).contains(&imm) || (0xfffe0..=0xfffff).contains(&imm))
            {
                return Some(InstCall {
                    mnemonic: "c.lui".to_owned(),
                    operands: vec![rd_s.clone(), imm.to_string()],
                });
            }
        }
        "slli" if call.operands.len() == 3 => {
            let (rd_s, rs_s, sh_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs = i_reg_num(rs_s)?;
            let sh = eval_i(sh_s)?;
            let max_sh = if march.xlen == Xlen::Rv32 { 31 } else { 63 };
            if rd != 0 && rd == rs && (1..=max_sh).contains(&sh) {
                return Some(InstCall {
                    mnemonic: "c.slli".to_owned(),
                    operands: vec![rd_s.clone(), sh.to_string()],
                });
            }
        }
        "srli" | "srai" if call.operands.len() == 3 => {
            let (rd_s, rs_s, sh_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs = i_reg_num(rs_s)?;
            let sh = eval_i(sh_s)?;
            let max_sh = if march.xlen == Xlen::Rv32 { 31 } else { 63 };
            if is_c_gpr(rd) && rd == rs && (1..=max_sh).contains(&sh) {
                return Some(InstCall {
                    mnemonic: format!("c.{}", call.mnemonic),
                    operands: vec![rd_s.clone(), sh.to_string()],
                });
            }
        }
        "andi" if call.operands.len() == 3 => {
            let (rd_s, rs_s, imm_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs = i_reg_num(rs_s)?;
            let imm = eval_i(imm_s)?;
            if is_c_gpr(rd) && rd == rs && (-32..=31).contains(&imm) {
                return Some(InstCall {
                    mnemonic: "c.andi".to_owned(),
                    operands: vec![rd_s.clone(), imm.to_string()],
                });
            }
        }
        "sub" | "xor" | "or" | "and" | "subw" | "addw" if call.operands.len() == 3 => {
            let is_w = call.mnemonic.ends_with('w');
            if is_w && march.xlen == Xlen::Rv32 {
                return None;
            }
            let (rd_s, rs1_s, rs2_s) = (&call.operands[0], &call.operands[1], &call.operands[2]);
            let rd = i_reg_num(rd_s)?;
            let rs1 = i_reg_num(rs1_s)?;
            let rs2 = i_reg_num(rs2_s)?;
            if is_c_gpr(rd) && rd == rs1 && is_c_gpr(rs2) {
                return Some(InstCall {
                    mnemonic: format!("c.{}", call.mnemonic),
                    operands: vec![rd_s.clone(), rs2_s.clone()],
                });
            }
        }
        "lw" | "sw" | "ld" | "sd" if call.operands.len() == 2 => {
            let is_dbl = matches!(call.mnemonic.as_str(), "ld" | "sd");
            if is_dbl && march.xlen == Xlen::Rv32 {
                return None;
            }
            let rd = i_reg_num(&call.operands[0])?;
            let (off_s, base_s) = parse_mem_parts(&call.operands[1])?;
            let base = i_reg_num(base_s)?;
            let imm = if off_s.is_empty() { 0 } else { eval_i(off_s)? };
            let is_load = matches!(call.mnemonic.as_str(), "lw" | "ld");
            let sp_max = if is_dbl { 504 } else { 252 };
            let step = if is_dbl { 8 } else { 4 };
            let gp_max = if is_dbl { 248 } else { 124 };
            if base == 2
                && (!is_load || rd != 0)
                && (0..=sp_max).contains(&imm)
                && imm.rem_euclid(step) == 0
            {
                return Some(InstCall {
                    mnemonic: format!("c.{}sp", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}(sp)")],
                });
            }
            if is_c_gpr(rd)
                && is_c_gpr(base)
                && (0..=gp_max).contains(&imm)
                && imm.rem_euclid(step) == 0
            {
                return Some(InstCall {
                    mnemonic: format!("c.{}", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}({base_s})")],
                });
            }
        }
        "flw" | "fsw"
            if call.operands.len() == 2
                && march.xlen == Xlen::Rv32
                && march.extensions.contains("zcf") =>
        {
            let rd = f_reg_num(&call.operands[0])?;
            let (off_s, base_s) = parse_mem_parts(&call.operands[1])?;
            let base = i_reg_num(base_s)?;
            let imm = if off_s.is_empty() { 0 } else { eval_i(off_s)? };
            if base == 2 && (0..=252).contains(&imm) && imm.rem_euclid(4) == 0 {
                return Some(InstCall {
                    mnemonic: format!("c.{}sp", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}(sp)")],
                });
            }
            if is_c_fpr(rd) && is_c_gpr(base) && (0..=124).contains(&imm) && imm.rem_euclid(4) == 0
            {
                return Some(InstCall {
                    mnemonic: format!("c.{}", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}({base_s})")],
                });
            }
        }
        "fld" | "fsd" if call.operands.len() == 2 && march.extensions.contains("zcd") => {
            let rd = f_reg_num(&call.operands[0])?;
            let (off_s, base_s) = parse_mem_parts(&call.operands[1])?;
            let base = i_reg_num(base_s)?;
            let imm = if off_s.is_empty() { 0 } else { eval_i(off_s)? };
            if base == 2 && (0..=504).contains(&imm) && imm.rem_euclid(8) == 0 {
                return Some(InstCall {
                    mnemonic: format!("c.{}sp", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}(sp)")],
                });
            }
            if is_c_fpr(rd) && is_c_gpr(base) && (0..=248).contains(&imm) && imm.rem_euclid(8) == 0
            {
                return Some(InstCall {
                    mnemonic: format!("c.{}", call.mnemonic),
                    operands: vec![call.operands[0].clone(), format!("{imm}({base_s})")],
                });
            }
        }
        "jalr" if call.operands.len() == 2 || call.operands.len() == 3 => {
            let rd = i_reg_num(&call.operands[0])?;
            let (imm_opt, rs1_s) = if call.operands.len() == 2 {
                let (off, base) = parse_mem_parts(&call.operands[1])?;
                let iv = if off.is_empty() { 0 } else { eval_i(off)? };
                (Some(iv), base)
            } else {
                (eval_i(&call.operands[2]), call.operands[1].as_str())
            };
            let rs1 = i_reg_num(rs1_s)?;
            if imm_opt == Some(0) && rs1 != 0 {
                if rd == 0 {
                    return Some(InstCall {
                        mnemonic: "c.jr".to_owned(),
                        operands: vec![rs1_s.trim().to_owned()],
                    });
                }
                if rd == 1 {
                    return Some(InstCall {
                        mnemonic: "c.jalr".to_owned(),
                        operands: vec![rs1_s.trim().to_owned()],
                    });
                }
            }
        }
        "jal" if call.operands.len() == 2 => {
            let rd = reg_num(&call.operands[0])?;
            if let Some(imm) = eval_i(&call.operands[1])
                && (-2048..=2046).contains(&imm)
                && imm.rem_euclid(2) == 0
            {
                if rd == 0 {
                    return Some(InstCall {
                        mnemonic: "c.j".to_owned(),
                        operands: vec![imm.to_string()],
                    });
                }
                if rd == 1 && march.xlen == Xlen::Rv32 {
                    return Some(InstCall {
                        mnemonic: "c.jal".to_owned(),
                        operands: vec![imm.to_string()],
                    });
                }
            }
        }
        "beq" | "bne" if call.operands.len() == 3 => {
            let rs1 = i_reg_num(&call.operands[0])?;
            let rs2 = i_reg_num(&call.operands[1])?;
            if let Some(imm) = eval_i(&call.operands[2])
                && (-256..=254).contains(&imm)
                && imm.rem_euclid(2) == 0
            {
                let cmp_mnemonic = if call.mnemonic == "beq" {
                    "c.beqz"
                } else {
                    "c.bnez"
                };
                if rs2 == 0 && is_c_gpr(rs1) {
                    return Some(InstCall {
                        mnemonic: cmp_mnemonic.to_owned(),
                        operands: vec![call.operands[0].clone(), imm.to_string()],
                    });
                }
                if rs1 == 0 && is_c_gpr(rs2) {
                    return Some(InstCall {
                        mnemonic: cmp_mnemonic.to_owned(),
                        operands: vec![call.operands[1].clone(), imm.to_string()],
                    });
                }
            }
        }
        "ebreak" if call.operands.is_empty() => {
            return Some(InstCall {
                mnemonic: "c.ebreak".to_owned(),
                operands: Vec::new(),
            });
        }
        _ => {}
    }

    None
}

fn encode_call(
    call: &InstCall,
    march: &MarchSpec,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<String> {
    let inst = db.instruction(&call.mnemonic)?;
    for v in &inst.variants {
        if !march.supports_extensions(&v.required_extensions, v.xlen) {
            continue;
        }
        let Some(tmpl) = &v.encoding else {
            continue;
        };
        let Some(bindings) = bind_variant_operands(&v.syntax, v, &call.operands) else {
            continue;
        };

        let mut word = tmpl.fixed;
        let mut relocated = false;
        let mut ok = true;

        for f in &tmpl.fields {
            let op_class = v
                .operands
                .iter()
                .find(|o| o.name == f.operand)
                .map_or("", |o| o.class.as_str());

            let min_val_lo = tmpl
                .fields
                .iter()
                .filter(|other| other.operand == f.operand)
                .map(|other| other.val_lo)
                .min()
                .unwrap_or(0);
            let align_zeros = lsb_align_zeros(op_class);
            let pre_shift = align_zeros.saturating_sub(min_val_lo);

            let val_opt = if let Some(raw_val) = bindings.get(&f.operand) {
                encode_operand_value(raw_val, op_class, pre_shift, known_consts, db)
            } else if op_class == "VMaskOp" {
                Some(EncodedOperand::Bits(1))
            } else if matches!(op_class, "frmarg" | "frmarglegacy") {
                Some(EncodedOperand::Bits(7))
            } else {
                None
            };

            match val_opt {
                Some(EncodedOperand::Bits(val)) => {
                    let mask = if f.width >= 32 {
                        u32::MAX
                    } else {
                        (1u32 << f.width) - 1
                    };
                    let slice = (val >> f.val_lo) & mask;
                    word |= slice << f.inst_lo;
                }
                Some(EncodedOperand::Relocated) => {
                    relocated = true;
                    break;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }

        if relocated {
            return Some("(relocated)".to_owned());
        }
        if ok {
            return match tmpl.size {
                2 => Some(format!("0x{:04x}", word as u16)),
                4 => Some(format!("0x{word:08x}")),
                _ => None,
            };
        }
    }

    None
}

enum EncodedOperand {
    Bits(u32),
    Relocated,
}

fn lsb_align_zeros(class: &str) -> u8 {
    let Some(idx) = class.find("_lsb") else {
        return 0;
    };
    let zeros = class[idx + 4..].bytes().take_while(|&b| b == b'0').count();
    u8::try_from(zeros).unwrap_or(0)
}

fn encode_operand_value(
    raw: &str,
    class: &str,
    pre_shift: u8,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<EncodedOperand> {
    let tok = raw.trim();
    match class {
        "csr_sysreg" => {
            if let Some(csr) = db.csr(tok) {
                return Some(EncodedOperand::Bits(u32::from(csr.number)));
            }
            if let Some(v) = eval_i128_const_expr(tok, known_consts) {
                return u32::try_from(v).ok().map(EncodedOperand::Bits);
            }
            return None;
        }
        "frmarg" | "frmarglegacy" | "rtzarg" => {
            let code = match tok.to_ascii_lowercase().as_str() {
                "rne" => 0,
                "rtz" => 1,
                "rdn" => 2,
                "rup" => 3,
                "rmm" => 4,
                "dyn" => 7,
                _ => return None,
            };
            return Some(EncodedOperand::Bits(code));
        }
        "VMaskOp" => {
            return if tok.eq_ignore_ascii_case("v0.t") {
                Some(EncodedOperand::Bits(0))
            } else {
                None
            };
        }
        "VMaskCarryInOp" => return Some(EncodedOperand::Bits(0)),
        "fencearg" => {
            let mut bits = 0u32;
            for b in tok.bytes() {
                bits |= match b.to_ascii_lowercase() {
                    b'i' => 8,
                    b'o' => 4,
                    b'r' => 2,
                    b'w' => 1,
                    _ => return None,
                };
            }
            return Some(EncodedOperand::Bits(bits));
        }
        "bare_symbol"
        | "call_symbol"
        | "pseudo_jump_symbol"
        | "tlsdesc_call_symbol"
        | "tprel_add_symbol" => return Some(EncodedOperand::Relocated),
        "SR07" => {
            let reg = db.register(tok)?;
            let enc = match reg.number {
                8 => 0,
                9 => 1,
                18..=23 => u32::from(reg.number - 16),
                _ => return None,
            };
            return Some(EncodedOperand::Bits(enc));
        }
        _ => {}
    }

    let is_immediate_only_class = class.starts_with("simm")
        || class.starts_with("uimm")
        || class.starts_with("bare_simm")
        || matches!(
            class,
            "c_lui_imm"
                | "imm5_zibi"
                | "stackadj"
                | "negstackadj"
                | "ixlenimm_li"
                | "ixlenimm_li_restricted"
                | "rnum"
                | "byteselect"
                | "TileLambdaOp"
                | "ybndsw_imm"
        );

    if !is_immediate_only_class && let Some(reg) = db.register(tok) {
        return Some(EncodedOperand::Bits(u32::from(reg.number)));
    }

    if db.register(tok).is_some() && is_immediate_only_class {
        return None;
    }

    if tok.contains('%') {
        return Some(EncodedOperand::Relocated);
    }

    if let Some(val) = eval_i128_const_expr(tok, known_consts) {
        let adjusted = if class.contains("_plus1") {
            val - 1
        } else {
            val
        };
        let shifted = if pre_shift > 0 {
            adjusted >> pre_shift
        } else {
            adjusted
        };
        return Some(EncodedOperand::Bits(shifted as u64 as u32));
    }

    // Unresolved symbol/label in immediate/branch offset -> relocated
    Some(EncodedOperand::Relocated)
}

fn bind_typed_variant_operands(
    variant: &InstructionVariant,
    user_ops: &[String],
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> Option<HashMap<String, String>> {
    let bindings = bind_variant_operands(&variant.syntax, variant, user_ops)?;
    for op in &variant.operands {
        let Some(val) = bindings.get(&op.name) else {
            continue;
        };
        if !operand_fits_coarse_class(val, &op.class, known_consts, db) {
            return None;
        }
    }
    Some(bindings)
}

fn operand_fits_coarse_class(
    tok: &str,
    class: &str,
    known_consts: &HashMap<String, String>,
    db: &Database,
) -> bool {
    let t = tok.trim();
    let is_reg = db.register(t).is_some();
    if matches!(
        class,
        "GPR"
            | "GPRC"
            | "GPRMem"
            | "GPRCMem"
            | "BasePtr"
            | "BasePtrC"
            | "GPRMemZeroOffset"
            | "GPRNoX0"
            | "GPRNoX2"
            | "GPRNoX0X2"
            | "GPRPairRV32"
            | "GPRPairCRV32"
            | "GPRPairNoX0RV32"
            | "GPRPairRV64"
            | "GPRX1"
            | "GPRX1X5"
            | "GPRX5"
            | "SP"
            | "SPMem"
            | "SR07"
            | "YGPR"
            | "FPR16"
            | "FPR16INX"
            | "FPR32"
            | "FPR32C"
            | "FPR32INX"
            | "FPR64"
            | "FPR64C"
            | "FPR64IN32X"
            | "FPR64INX"
            | "FPR128"
            | "VR"
            | "VRNoV0"
            | "VRM2"
            | "VRM4"
            | "VRM8"
    ) {
        return is_reg;
    }
    if class.starts_with("simm")
        || class.starts_with("uimm")
        || class.starts_with("bare_simm")
        || matches!(
            class,
            "c_lui_imm"
                | "imm5_zibi"
                | "stackadj"
                | "negstackadj"
                | "ixlenimm_li"
                | "ixlenimm_li_restricted"
        )
    {
        return !is_reg || eval_i128_const_expr(t, known_consts).is_some();
    }
    true
}

fn bind_variant_operands(
    syntax: &str,
    variant: &InstructionVariant,
    user_ops: &[String],
) -> Option<HashMap<String, String>> {
    let (_head, rest) = parse_syntax_parts(syntax)?;
    if rest.is_empty() {
        return user_ops.is_empty().then(HashMap::new);
    }
    let slots = split_top_level_commas(rest)?;
    let optional_tail = variant
        .operands
        .last()
        .is_some_and(|op| matches!(op.class.as_str(), "VMaskOp" | "frmarg" | "frmarglegacy"));
    let min_ops = if optional_tail {
        slots.len().saturating_sub(1)
    } else {
        slots.len()
    };
    if user_ops.len() < min_ops || user_ops.len() > slots.len() {
        return None;
    }

    let mut out = HashMap::new();
    for (idx, slot) in slots.iter().enumerate() {
        let slot_t = slot.trim();
        let Some(user) = user_ops.get(idx).map(|s| s.trim()) else {
            continue;
        };
        if let Some((slot_off, slot_base)) = parse_mem_parts(slot_t) {
            let (user_off, user_base) = parse_mem_parts(user)?;
            let base_name = slot_base.trim();
            out.insert(base_name.to_owned(), user_base.trim().to_owned());
            if !slot_off.is_empty() {
                let val = if user_off.is_empty() { "0" } else { user_off };
                out.insert(slot_off.to_owned(), val.to_owned());
            } else if !user_off.is_empty() && user_off != "0" {
                return None;
            }
        } else {
            out.insert(slot_t.to_owned(), user.to_owned());
        }
    }
    Some(out)
}

fn render_target_operands(
    target_syntax: &str,
    bindings: &HashMap<String, String>,
) -> Option<Vec<String>> {
    let (_head, rest) = parse_syntax_parts(target_syntax)?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let slots = split_top_level_commas(rest)?;
    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        let s = slot.trim();
        if let Some((off_name, base_name)) = parse_mem_parts(s) {
            let base_val = bindings.get(base_name.trim())?;
            if off_name.is_empty() {
                out.push(format!("({base_val})"));
            } else {
                let off_val = bindings.get(off_name)?;
                out.push(format!("{off_val}({base_val})"));
            }
        } else if let Some(val) = bindings.get(s) {
            out.push(val.clone());
        }
    }
    Some(out)
}

fn strip_leading_labels(s: &str) -> &str {
    let mut cur = s.trim_start();
    loop {
        let bytes = cur.as_bytes();
        let mut i = 0;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'$'))
        {
            i += 1;
        }
        if i > 0 && bytes.get(i) == Some(&b':') {
            cur = cur[i + 1..].trim_start();
        } else {
            return cur;
        }
    }
}

fn parse_syntax_parts(s: &str) -> Option<(&str, &str)> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    Some(match t.find(char::is_whitespace) {
        Some(idx) => (&t[..idx], t[idx..].trim_start()),
        None => (t, ""),
    })
}

fn parse_stmt(stmt: &str) -> Option<(&str, Vec<String>)> {
    let (mnem, rest) = parse_syntax_parts(stmt)?;
    if rest.is_empty() {
        return Some((mnem, Vec::new()));
    }
    let ops = split_top_level_commas(rest)?;
    Some((mnem, ops))
}

fn split_top_level_commas(rest: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut paren = 0usize;
    let mut brace = 0usize;
    let mut start = 0usize;
    for (i, b) in rest.bytes().enumerate() {
        match b {
            b'(' => paren += 1,
            b')' => {
                if paren == 0 {
                    return None;
                }
                paren -= 1;
            }
            b'{' => brace += 1,
            b'}' => {
                if brace == 0 {
                    return None;
                }
                brace -= 1;
            }
            b',' if paren == 0 && brace == 0 => {
                out.push(rest[start..i].trim().to_owned());
                start = i + 1;
            }
            _ => {}
        }
    }
    if paren != 0 || brace != 0 {
        return None;
    }
    out.push(rest[start..].trim().to_owned());
    Some(out)
}

fn parse_mem_parts(s: &str) -> Option<(&str, &str)> {
    let t = s.trim();
    if !t.ends_with(')') {
        return None;
    }
    let bytes = t.as_bytes();
    let mut depth = 0usize;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some((t[..i].trim(), t[i + 1..bytes.len() - 1].trim()));
                }
            }
            _ => {}
        }
    }
    None
}

fn eval_u64_const_expr(expr: &str, known_consts: &HashMap<String, String>) -> Option<u64> {
    let resolved = if known_consts.is_empty() {
        expr.to_owned()
    } else {
        substitute_constants(expr, known_consts)
    };
    let val = eval_raw_i128(&resolved)?;
    if val < 0 {
        i64::try_from(val).ok().map(|v| v as u64)
    } else {
        u64::try_from(val).ok()
    }
}

fn eval_i128_const_expr(expr: &str, known_consts: &HashMap<String, String>) -> Option<i128> {
    let resolved = if known_consts.is_empty() {
        expr.to_owned()
    } else {
        substitute_constants(expr, known_consts)
    };
    eval_raw_i128(&resolved)
}

fn eval_raw_i128(s: &str) -> Option<i128> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let toks = tokenize_expr(t)?;
    let mut pos = 0;
    let val = parse_bitor(&toks, &mut pos, 0)?;
    if pos == toks.len() { Some(val) } else { None }
}

#[derive(Debug, Clone, Copy)]
enum ExprTok {
    Int(i128),
    Plus,
    Minus,
    Not,
    BitNot,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    And,
    Xor,
    Or,
    LParen,
    RParen,
}

fn tokenize_expr(s: &str) -> Option<Vec<ExprTok>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let clean = s[i..j].trim_end_matches(['u', 'U', 'l', 'L']);
            let val = if let Some(h) = clean
                .strip_prefix("0x")
                .or_else(|| clean.strip_prefix("0X"))
            {
                i128::from_str_radix(h, 16).ok()?
            } else if let Some(bin) = clean
                .strip_prefix("0b")
                .or_else(|| clean.strip_prefix("0B"))
            {
                i128::from_str_radix(bin, 2).ok()?
            } else if clean.starts_with('0') && clean.len() > 1 {
                i128::from_str_radix(&clean[1..], 8).ok()?
            } else {
                clean.parse::<i128>().ok()?
            };
            out.push(ExprTok::Int(val));
            i = j;
            continue;
        }
        if b == b'<' && bytes.get(i + 1) == Some(&b'<') {
            out.push(ExprTok::Shl);
            i += 2;
            continue;
        }
        if b == b'>' && bytes.get(i + 1) == Some(&b'>') {
            out.push(ExprTok::Shr);
            i += 2;
            continue;
        }
        let tok = match b {
            b'+' => ExprTok::Plus,
            b'-' => ExprTok::Minus,
            b'!' => ExprTok::Not,
            b'~' => ExprTok::BitNot,
            b'*' => ExprTok::Mul,
            b'/' => ExprTok::Div,
            b'%' => ExprTok::Rem,
            b'&' => ExprTok::And,
            b'^' => ExprTok::Xor,
            b'|' => ExprTok::Or,
            b'(' => ExprTok::LParen,
            b')' => ExprTok::RParen,
            _ => return None,
        };
        out.push(tok);
        i += 1;
    }
    Some(out)
}

fn parse_bitor(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_bitxor(toks, pos, depth)?;
    while matches!(toks.get(*pos), Some(ExprTok::Or)) {
        *pos += 1;
        lhs |= parse_bitxor(toks, pos, depth)?;
    }
    Some(lhs)
}

fn parse_bitxor(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_bitand(toks, pos, depth)?;
    while matches!(toks.get(*pos), Some(ExprTok::Xor)) {
        *pos += 1;
        lhs ^= parse_bitand(toks, pos, depth)?;
    }
    Some(lhs)
}

fn parse_bitand(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_shift(toks, pos, depth)?;
    while matches!(toks.get(*pos), Some(ExprTok::And)) {
        *pos += 1;
        lhs &= parse_shift(toks, pos, depth)?;
    }
    Some(lhs)
}

fn parse_shift(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_add(toks, pos, depth)?;
    loop {
        match toks.get(*pos) {
            Some(ExprTok::Shl) => {
                *pos += 1;
                let rhs = u32::try_from(parse_add(toks, pos, depth)?).ok()?;
                lhs = lhs.wrapping_shl(rhs);
            }
            Some(ExprTok::Shr) => {
                *pos += 1;
                let rhs = u32::try_from(parse_add(toks, pos, depth)?).ok()?;
                lhs = lhs.wrapping_shr(rhs);
            }
            _ => break,
        }
    }
    Some(lhs)
}

fn parse_add(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_mul(toks, pos, depth)?;
    loop {
        match toks.get(*pos) {
            Some(ExprTok::Plus) => {
                *pos += 1;
                lhs = lhs.wrapping_add(parse_mul(toks, pos, depth)?);
            }
            Some(ExprTok::Minus) => {
                *pos += 1;
                lhs = lhs.wrapping_sub(parse_mul(toks, pos, depth)?);
            }
            _ => break,
        }
    }
    Some(lhs)
}

fn parse_mul(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    let mut lhs = parse_unary(toks, pos, depth)?;
    loop {
        match toks.get(*pos) {
            Some(ExprTok::Mul) => {
                *pos += 1;
                lhs = lhs.wrapping_mul(parse_unary(toks, pos, depth)?);
            }
            Some(ExprTok::Div) => {
                *pos += 1;
                lhs = lhs.checked_div(parse_unary(toks, pos, depth)?)?;
            }
            Some(ExprTok::Rem) => {
                *pos += 1;
                lhs = lhs.checked_rem(parse_unary(toks, pos, depth)?)?;
            }
            _ => break,
        }
    }
    Some(lhs)
}

fn parse_unary(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    if depth >= 64 {
        return None;
    }
    match toks.get(*pos) {
        Some(ExprTok::Plus) => {
            *pos += 1;
            parse_unary(toks, pos, depth + 1)
        }
        Some(ExprTok::Minus) => {
            *pos += 1;
            Some(parse_unary(toks, pos, depth + 1)?.wrapping_neg())
        }
        Some(ExprTok::BitNot) => {
            *pos += 1;
            Some(!parse_unary(toks, pos, depth + 1)?)
        }
        Some(ExprTok::Not) => {
            *pos += 1;
            Some(i128::from(parse_unary(toks, pos, depth + 1)? == 0))
        }
        _ => parse_primary(toks, pos, depth + 1),
    }
}

fn parse_primary(toks: &[ExprTok], pos: &mut usize, depth: usize) -> Option<i128> {
    match toks.get(*pos).copied()? {
        ExprTok::LParen => {
            *pos += 1;
            let v = parse_bitor(toks, pos, depth)?;
            if !matches!(toks.get(*pos), Some(ExprTok::RParen)) {
                return None;
            }
            *pos += 1;
            Some(v)
        }
        ExprTok::Int(v) => {
            *pos += 1;
            Some(v)
        }
        _ => None,
    }
}
