use crate::context::MarchSpec;
use crate::data::{Database, Xlen};

/// Generate RISC-V predefined C preprocessor macros matching `clang -E -dM -x assembler-with-cpp`.
pub fn predefined_macros(
    march: &MarchSpec,
    abi: Option<&str>,
    cmodel: Option<&str>,
    db: &Database,
) -> Vec<(String, String)> {
    let mut macros = vec![
        ("__ASSEMBLER__".to_owned(), "1".to_owned()),
        ("__riscv".to_owned(), "1".to_owned()),
        ("__riscv_arch_test".to_owned(), "1".to_owned()),
        ("__riscv_misaligned_avoid".to_owned(), "1".to_owned()),
        ("__riscv_v_intrinsic".to_owned(), "1000000".to_owned()),
    ];

    let xlen_val = match march.xlen {
        Xlen::Rv32 => "32",
        Xlen::Rv64 | Xlen::Both => "64",
    };
    macros.push(("__riscv_xlen".to_owned(), xlen_val.to_owned()));

    let has = |ext: &str| march.extensions.contains(ext);

    if has("e") {
        let key = if march.xlen == Xlen::Rv32 {
            "__riscv_32e"
        } else {
            "__riscv_64e"
        };
        macros.push((key.to_owned(), "1".to_owned()));
    }

    let eff_abi = effective_abi(march, abi);
    match eff_abi.as_str() {
        "ilp32e" | "lp64e" => {
            macros.push(("__riscv_abi_rve".to_owned(), "1".to_owned()));
            macros.push(("__riscv_float_abi_soft".to_owned(), "1".to_owned()));
        }
        "ilp32f" | "lp64f" => {
            macros.push(("__riscv_float_abi_single".to_owned(), "1".to_owned()));
        }
        "ilp32d" | "lp64d" => {
            macros.push(("__riscv_float_abi_double".to_owned(), "1".to_owned()));
        }
        "lp64q" => {
            macros.push(("__riscv_float_abi_quad".to_owned(), "1".to_owned()));
        }
        _ => {
            macros.push(("__riscv_float_abi_soft".to_owned(), "1".to_owned()));
        }
    }

    let cmodel_macro = match cmodel.map(str::to_ascii_lowercase).as_deref() {
        Some("medany") => "__riscv_cmodel_medany",
        Some("large") => "__riscv_cmodel_large",
        _ => "__riscv_cmodel_medlow",
    };
    macros.push((cmodel_macro.to_owned(), "1".to_owned()));

    if has("q") {
        macros.push(("__riscv_flen".to_owned(), "128".to_owned()));
    } else if has("d") {
        macros.push(("__riscv_flen".to_owned(), "64".to_owned()));
    } else if has("f") {
        macros.push(("__riscv_flen".to_owned(), "32".to_owned()));
    }

    if has("f") {
        macros.push(("__riscv_fdiv".to_owned(), "1".to_owned()));
        macros.push(("__riscv_fsqrt".to_owned(), "1".to_owned()));
    }

    if has("m") || has("zmmul") {
        macros.push(("__riscv_mul".to_owned(), "1".to_owned()));
    }
    if has("m") {
        macros.push(("__riscv_div".to_owned(), "1".to_owned()));
        macros.push(("__riscv_muldiv".to_owned(), "1".to_owned()));
    }
    if has("a") {
        macros.push(("__riscv_atomic".to_owned(), "1".to_owned()));
    }
    if has("c") {
        macros.push(("__riscv_compressed".to_owned(), "1".to_owned()));
    }

    if has("v") || march.extensions.iter().any(|e| e.starts_with("zve")) {
        macros.push(("__riscv_vector".to_owned(), "1".to_owned()));
        let elen = if has("zve64x") { "64" } else { "32" };
        macros.push(("__riscv_v_elen".to_owned(), elen.to_owned()));
        let elen_fp = if has("zve64d") {
            "64"
        } else if has("zve32f") {
            "32"
        } else {
            "0"
        };
        macros.push(("__riscv_v_elen_fp".to_owned(), elen_fp.to_owned()));
        if let Some(min_vlen) = march
            .extensions
            .iter()
            .filter_map(|e| {
                e.strip_prefix("zvl")?
                    .strip_suffix('b')?
                    .parse::<u32>()
                    .ok()
            })
            .max()
        {
            macros.push(("__riscv_v_min_vlen".to_owned(), min_vlen.to_string()));
        }
    }

    for ext in &march.extensions {
        if let Some(entry) = db.extension(ext)
            && let Some(ver_str) = entry.version.as_deref()
            && let Some(ver_val) = encode_extension_version(ver_str)
        {
            macros.push((format!("__riscv_{ext}"), ver_val.to_string()));
        }
    }

    macros
}

fn effective_abi(march: &MarchSpec, explicit_abi: Option<&str>) -> String {
    if let Some(abi) = explicit_abi.filter(|s| !s.is_empty()) {
        return abi.to_ascii_lowercase();
    }
    let prefix = if march.xlen == Xlen::Rv32 {
        "ilp32"
    } else {
        "lp64"
    };
    if march.extensions.contains("e") {
        format!("{prefix}e")
    } else if march.extensions.contains("d") {
        format!("{prefix}d")
    } else if march.extensions.contains("f") {
        format!("{prefix}f")
    } else {
        prefix.to_owned()
    }
}

fn encode_extension_version(version: &str) -> Option<u64> {
    let mut parts = version.trim().split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some(major * 1_000_000 + minor * 1_000 + patch)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn to_map(items: Vec<(String, String)>) -> HashMap<String, String> {
        items.into_iter().collect()
    }

    #[test]
    fn generates_expected_predefined_macros_for_rv64gc_and_rv32e() {
        let db = Database::load().unwrap();
        let gc = MarchSpec::parse("rv64gc");
        let gc_map = to_map(predefined_macros(&gc, Some("lp64d"), None, &db));
        assert_eq!(gc_map.get("__ASSEMBLER__").map(String::as_str), Some("1"));
        assert_eq!(gc_map.get("__riscv").map(String::as_str), Some("1"));
        assert_eq!(gc_map.get("__riscv_xlen").map(String::as_str), Some("64"));
        assert_eq!(gc_map.get("__riscv_flen").map(String::as_str), Some("64"));
        assert_eq!(
            gc_map.get("__riscv_float_abi_double").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            gc_map.get("__riscv_cmodel_medlow").map(String::as_str),
            Some("1")
        );
        assert_eq!(gc_map.get("__riscv_atomic").map(String::as_str), Some("1"));
        assert_eq!(
            gc_map.get("__riscv_compressed").map(String::as_str),
            Some("1")
        );
        assert_eq!(gc_map.get("__riscv_i").map(String::as_str), Some("2001000"));

        let rve = MarchSpec::parse("rv32e");
        let rve_map = to_map(predefined_macros(&rve, None, Some("medany"), &db));
        assert_eq!(rve_map.get("__riscv_xlen").map(String::as_str), Some("32"));
        assert_eq!(rve_map.get("__riscv_32e").map(String::as_str), Some("1"));
        assert_eq!(
            rve_map.get("__riscv_abi_rve").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            rve_map.get("__riscv_cmodel_medany").map(String::as_str),
            Some("1")
        );
    }
}
