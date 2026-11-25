use crate::data::Database;

/// Decode an LLVM operand class name (77 total: 51 register/special classes from `data/texts.json`
/// plus 26 mechanically decoded immediate constraint classes).
pub fn describe_operand_class(class: &str, db: &Database) -> Option<String> {
    if let Some(desc) = db.texts.operand_classes.get(class) {
        return Some(desc.clone());
    }
    decode_immediate_class(class)
}

fn decode_immediate_class(class: &str) -> Option<String> {
    match class {
        "uimmlog2xlen" => return Some("unsigned, 0..XLEN-1".to_owned()),
        "uimmlog2xlennonzero" => return Some("unsigned, 1..XLEN-1".to_owned()),
        "uimm8ge32" => return Some("unsigned 8-bit (32..255)".to_owned()),
        _ => {}
    }

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

    let sign_label = if signed { "signed" } else { "unsigned" };
    let mut desc = format!("{sign_label} {bits}-bit");
    if is_pc_rel {
        desc.push_str(" PC-rel");
    }
    if step > 1 {
        desc.push_str(&format!(", {step}-byte aligned"));
    }
    if plus1 {
        desc.push_str(" + 1");
    }
    if nonzero {
        desc.push_str(", nonzero");
    }
    if signed && nonzero && !plus1 {
        desc.push_str(&format!(" ({min_out}..{max_val}, ≠ 0)"));
    } else {
        desc.push_str(&format!(" ({min_out}..{max_val})"));
    }
    Some(desc)
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
