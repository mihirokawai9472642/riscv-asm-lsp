use crate::data::{Database, Xlen};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImmSpec {
    pub signed: bool,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub nonzero: bool,
}

struct ParsedImmParts {
    signed: bool,
    bits: u32,
    is_pc_rel: bool,
    plus1: bool,
    nonzero: bool,
    step: i64,
    min: i64,
    max: i64,
}

/// Decode an LLVM operand class name (77 total: 51 register/special classes from `server/data/texts.json`
/// plus 26 mechanically decoded immediate constraint classes).
pub fn describe_operand_class(class: &str, db: &Database) -> Option<String> {
    if let Some(desc) = db.texts.operand_classes.get(class) {
        return Some(desc.clone());
    }
    decode_immediate_class(class)
}

pub(crate) fn parse_immediate_spec(class: &str, xlen: Xlen) -> Option<ImmSpec> {
    let shamt_max = if xlen == Xlen::Rv32 { 31 } else { 63 };
    let eq_xlen = if xlen == Xlen::Rv32 { 32 } else { 64 };
    match class {
        "uimmlog2xlen" => {
            return Some(ImmSpec {
                signed: false,
                min: 0,
                max: shamt_max,
                step: 1,
                nonzero: false,
            });
        }
        "uimmlog2xlennonzero" => {
            return Some(ImmSpec {
                signed: false,
                min: 1,
                max: shamt_max,
                step: 1,
                nonzero: true,
            });
        }
        "uimm8ge32" => {
            return Some(ImmSpec {
                signed: false,
                min: 32,
                max: 255,
                step: 1,
                nonzero: false,
            });
        }
        "rnum" => {
            return Some(ImmSpec {
                signed: false,
                min: 0,
                max: 10,
                step: 1,
                nonzero: false,
            });
        }
        "byteselect" => {
            return Some(ImmSpec {
                signed: false,
                min: 0,
                max: 3,
                step: 1,
                nonzero: false,
            });
        }
        "TileLambdaOp" => {
            return Some(ImmSpec {
                signed: false,
                min: 0,
                max: 7,
                step: 1,
                nonzero: false,
            });
        }
        "uimm7_eq_xlen" => {
            return Some(ImmSpec {
                signed: false,
                min: eq_xlen,
                max: eq_xlen,
                step: 1,
                nonzero: false,
            });
        }
        "ybndsw_imm" => {
            return Some(ImmSpec {
                signed: false,
                min: 0,
                max: 511,
                step: 1,
                nonzero: false,
            });
        }
        _ => {}
    }

    let p = parse_simm_uimm_parts(class)?;
    Some(ImmSpec {
        signed: p.signed,
        min: p.min,
        max: p.max,
        step: p.step,
        nonzero: p.nonzero,
    })
}

fn decode_immediate_class(class: &str) -> Option<String> {
    match class {
        "uimmlog2xlen" => return Some("unsigned, 0..XLEN-1".to_owned()),
        "uimmlog2xlennonzero" => return Some("unsigned, 1..XLEN-1".to_owned()),
        "uimm8ge32" => return Some("unsigned 8-bit (32..255)".to_owned()),
        _ => {}
    }

    let p = parse_simm_uimm_parts(class)?;
    let sign_label = if p.signed { "signed" } else { "unsigned" };
    let mut desc = format!("{sign_label} {}-bit", p.bits);
    if p.is_pc_rel {
        desc.push_str(" PC-rel");
    }
    if p.step > 1 {
        desc.push_str(&format!(", {}-byte aligned", p.step));
    }
    if p.plus1 {
        desc.push_str(" + 1");
    }
    if p.nonzero {
        desc.push_str(", nonzero");
    }
    if p.signed && p.nonzero && !p.plus1 {
        desc.push_str(&format!(" ({}..{}, ≠ 0)", p.min, p.max));
    } else {
        desc.push_str(&format!(" ({}..{})", p.min, p.max));
    }
    Some(desc)
}

fn parse_simm_uimm_parts(class: &str) -> Option<ParsedImmParts> {
    let normalized = class.strip_prefix("bare_").unwrap_or(class);
    let (signed, mut rest) = if let Some(r) = normalized.strip_prefix("simm") {
        (true, r)
    } else {
        (false, normalized.strip_prefix("uimm")?)
    };

    let mut is_pc_rel = false;
    for suffix in ["_jal", "_auipc"] {
        if let Some(r) = rest.strip_suffix(suffix) {
            is_pc_rel = true;
            rest = r;
            break;
        }
    }
    for suffix in ["_lui", "_lo", "_pli_b", "_pli_h", "_pli_w", "_plui"] {
        if let Some(r) = rest.strip_suffix(suffix) {
            rest = r;
            break;
        }
    }

    let plus1 = if let Some(r) = rest.strip_suffix("_plus1") {
        rest = r;
        true
    } else {
        false
    };

    let nonzero = if let Some(r) = rest.strip_suffix("nonzero") {
        rest = r;
        true
    } else {
        false
    };

    let (bits_str, align_zeros) = if let Some((b, lsb)) = rest.split_once("_lsb") {
        if lsb.is_empty() || !lsb.bytes().all(|c| c == b'0') {
            return None;
        }
        (b, lsb.len() as u32)
    } else {
        (rest, 0)
    };

    let bits: u32 = bits_str.parse().ok()?;
    if bits == 0 || bits > 32 || align_zeros >= bits {
        return None;
    }

    let step = 1i64 << align_zeros;
    let (min_val, mut max_val) = if signed {
        let lo = -(1i64 << (bits - 1));
        let hi = (1i64 << (bits - 1)) - 1;
        (lo, hi & !(step - 1))
    } else {
        let hi = (1i64 << bits) - 1;
        (if nonzero { step } else { 0 }, hi & !(step - 1))
    };

    let min_out = if plus1 {
        max_val += 1;
        min_val + 1
    } else {
        min_val
    };

    Some(ParsedImmParts {
        signed,
        bits,
        is_pc_rel,
        plus1,
        nonzero,
        step,
        min: min_out,
        max: max_val,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn decodes_all_operand_classes_in_embedded_database() {
        let db = Database::load().expect("db");
        let mut all_classes = BTreeSet::new();
        for inst in &db.raw.instructions {
            for v in &inst.variants {
                for op in &v.operands {
                    all_classes.insert(op.class.as_str());
                }
            }
        }
        assert!(all_classes.len() >= 77, "expected >= 77 classes");
        let mut missing = Vec::new();
        for cls in &all_classes {
            if describe_operand_class(cls, &db).is_none() {
                missing.push(*cls);
            }
        }
        assert!(missing.is_empty(), "undecoded operand classes: {missing:?}");

        assert_eq!(
            describe_operand_class("simm12", &db).as_deref(),
            Some("signed 12-bit (-2048..2047)")
        );
        assert_eq!(
            describe_operand_class("simm12_lo", &db).as_deref(),
            Some("signed 12-bit (-2048..2047)")
        );
        assert_eq!(
            describe_operand_class("bare_simm13_lsb0", &db).as_deref(),
            Some("signed 13-bit, 2-byte aligned (-4096..4094)")
        );
        assert_eq!(
            describe_operand_class("simm10_pli_h", &db).as_deref(),
            Some("signed 10-bit (-512..511)")
        );
        assert_eq!(
            describe_operand_class("imm5_zibi", &db).as_deref(),
            Some("5-bit immediate (1..31 or -1)")
        );
        assert_eq!(
            describe_operand_class("uimm5", &db).as_deref(),
            Some("unsigned 5-bit (0..31)")
        );
        assert_eq!(
            describe_operand_class("simm13_lsb0", &db).as_deref(),
            Some("signed 13-bit, 2-byte aligned (-4096..4094)")
        );
        assert_eq!(
            describe_operand_class("uimm8_lsb000", &db).as_deref(),
            Some("unsigned 8-bit, 8-byte aligned (0..248)")
        );
        assert_eq!(
            describe_operand_class("simm6nonzero", &db).as_deref(),
            Some("signed 6-bit, nonzero (-32..31, ≠ 0)")
        );
        assert_eq!(
            describe_operand_class("simm5_plus1", &db).as_deref(),
            Some("signed 5-bit + 1 (-15..16)")
        );
        assert_eq!(
            describe_operand_class("uimmlog2xlen", &db).as_deref(),
            Some("unsigned, 0..XLEN-1")
        );
    }
}
