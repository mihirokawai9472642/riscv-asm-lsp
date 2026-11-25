mod compile_db;
mod march;
mod uri;

pub use compile_db::{
    CompileContext, FileContext, is_linker_script_template, is_linker_script_uri,
    is_preprocessed_by_suffix, is_riscv_target, split_command_args,
};
pub use march::{MarchSpec, effective_options_at_line};
#[cfg(test)]
pub(crate) use uri::test_uri;
pub use uri::{
    display_path_for_client, lexical_normalize_path, normalize_path, path_key, path_starts_with,
    path_strip_prefix, path_to_uri, uri_key, uri_to_path,
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use lsp_types::Uri;

    use crate::config::Config;
    use crate::data::Xlen;

    use super::compile_db::extract_relevant_flags;
    use super::*;

    #[test]
    fn parses_march_and_filters_extensions() {
        let rv64gc = MarchSpec::parse("rv64gc");
        assert_eq!(rv64gc.xlen, Xlen::Rv64);
        assert!(rv64gc.extensions.contains("i"));
        assert!(rv64gc.extensions.contains("m"));
        assert!(rv64gc.extensions.contains("a"));
        assert!(rv64gc.extensions.contains("f"));
        assert!(rv64gc.extensions.contains("d"));
        assert!(rv64gc.extensions.contains("c"));
        assert!(rv64gc.extensions.contains("zicsr"));
        assert!(!rv64gc.extensions.contains("v"));

        let rv32i = MarchSpec::parse("rv32i2p1_zba1p0");
        assert_eq!(rv32i.xlen, Xlen::Rv32);
        assert!(rv32i.extensions.contains("i"));
        assert!(rv32i.extensions.contains("zba"));
        assert!(!rv32i.extensions.contains("m"));
        assert!(!rv32i.supports_extensions(&["m".to_owned()], Xlen::Both));
        assert!(rv32i.supports_extensions(&["zba".to_owned()], Xlen::Rv32));
        assert!(!rv32i.supports_extensions(&["zba".to_owned()], Xlen::Rv64));
    }

    #[test]
    fn option_push_pop_tracks_rvc_and_arch() {
        let src = "\
            .option push\n\
            .option norvc\n\
            nop\n\
            .option pop\n\
            .option arch, +zba\n\
            sh1add a0, a1, a2\n";
        let base = MarchSpec::parse("rv64gc");
        let (mid, _) = effective_options_at_line(src, 2, &base);
        assert!(!mid.extensions.contains("c"));
        let (after_pop, _) = effective_options_at_line(src, 5, &base);
        assert!(after_pop.extensions.contains("c"));
        assert!(after_pop.extensions.contains("zba"));
    }

    #[test]
    fn quote_aware_command_splitter_handles_flags() {
        let args = split_command_args(
            r#"clang --target=riscv64-unknown-elf -march=rv64gc -I"my dir/inc" -DNAME=\"x\" -c foo.S"#,
        );
        assert_eq!(
            args,
            vec![
                "clang",
                "--target=riscv64-unknown-elf",
                "-march=rv64gc",
                "-Imy dir/inc",
                r#"-DNAME="x""#,
                "-c",
                "foo.S",
            ]
        );
    }

    #[test]
    fn uri_conversion_handles_localhost_special_chars_and_symlinks() {
        let round_path = std::env::temp_dir().join("ws dir+日本").join("a.S");
        let round_uri = path_to_uri(&round_path).expect("uri");
        assert_eq!(uri_to_path(&round_uri), Some(round_path.clone()));

        #[cfg(unix)]
        {
            let localhost_uri: Uri = "file://localhost/tmp/ws%20dir%2B%E6%97%A5%E6%9C%AC/a.S"
                .parse()
                .expect("uri");
            let decoded = uri_to_path(&localhost_uri).expect("file path");
            assert_eq!(decoded, PathBuf::from("/tmp/ws dir+日本/a.S"));

            let round_unix_uri = path_to_uri(Path::new("/tmp/ws dir+日本/a.S")).expect("uri");
            assert_eq!(uri_to_path(&round_unix_uri), Some(decoded));
        }

        #[cfg(windows)]
        {
            let upper: Uri = "file:///C:/Proj/Dir/a.S".parse().expect("uri");
            let encoded: Uri = "file:///c%3A/proj/dir/a.S".parse().expect("uri");
            assert_eq!(uri_key(&upper), uri_key(&encoded));

            let unc_uri: Uri = "file://server/share/Dir/a.S".parse().expect("unc uri");
            let unc_path = uri_to_path(&unc_uri).expect("unc path");
            assert_eq!(unc_path, PathBuf::from(r"\\server\share\Dir\a.S"));
            assert_eq!(path_to_uri(&unc_path), Some(unc_uri));
            assert!(path_starts_with(
                Path::new(r"\\server\share\DIR\a.S"),
                Path::new(r"\\server\share")
            ));
        }

        // Symlink path is preserved without canonicalization by path_to_uri & display_path_for_client
        #[cfg(unix)]
        {
            let base =
                std::env::temp_dir().join(format!("riscv_asm_lsp_sym_ctx_{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let real_dir = base.join("real dir");
            let link_dir = base.join("link");
            fs::create_dir_all(&real_dir).unwrap();
            fs::write(real_dir.join("a.S"), "nop\n").unwrap();
            std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
            let linked_file = link_dir.join("a.S");
            let sym_uri = path_to_uri(&linked_file).unwrap();
            assert!(
                sym_uri.as_str().contains("/link/a.S"),
                "got {}",
                sym_uri.as_str()
            );
            let mapped = display_path_for_client(
                &fs::canonicalize(&linked_file).unwrap(),
                Some(&link_dir),
                Some(&fs::canonicalize(&real_dir).unwrap()),
            );
            assert_eq!(mapped, linked_file);
            let _ = fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn extract_relevant_flags_allows_sysroot_include_imacros_and_wa_options() {
        let base = std::env::temp_dir().join("riscv_asm_flag_proj");
        let dir = base.join("build");
        let exp_asm_inc = lexical_normalize_path(&base.join("asm_inc"));
        let exp_pre = lexical_normalize_path(&base.join("inc").join("pre.h"));
        let exp_defs = lexical_normalize_path(&base.join("inc").join("defs.h"));
        let exp_sysroot = lexical_normalize_path(&base.join("sys"));
        let exp_sdk = lexical_normalize_path(&base.join("opt_sdk"));
        let isysroot_str = exp_sdk.to_string_lossy().into_owned();
        let args: Vec<String> = vec![
            "clang".to_owned(),
            "--target=riscv64-unknown-elf".to_owned(),
            "-march=rv64gc".to_owned(),
            "-O2".to_owned(),
            "-include".to_owned(),
            "../inc/pre.h".to_owned(),
            "-imacros../inc/defs.h".to_owned(),
            "--sysroot=../sys".to_owned(),
            "-isysroot".to_owned(),
            isysroot_str.clone(),
            "-nostdinc".to_owned(),
            "-Wa,-I../asm_inc,-march=rv64gcv,--fatal-warnings".to_owned(),
            "-c".to_owned(),
            "entry.S".to_owned(),
        ];

        let entry = extract_relevant_flags(&dir, &args);
        assert_eq!(entry.directory, lexical_normalize_path(&dir));
        assert_eq!(entry.march.as_deref(), Some("rv64gcv"));
        assert!(entry.include_dirs.contains(&exp_asm_inc));
        assert!(entry.clang_flags.contains(&"-include".to_owned()));
        assert!(
            entry
                .clang_flags
                .contains(&exp_pre.to_string_lossy().into_owned())
        );
        assert!(entry.clang_flags.contains(&"-imacros".to_owned()));
        assert!(
            entry
                .clang_flags
                .contains(&exp_defs.to_string_lossy().into_owned())
        );
        assert!(
            entry
                .clang_flags
                .contains(&format!("--sysroot={}", exp_sysroot.display()))
        );
        assert!(entry.clang_flags.contains(&"-isysroot".to_owned()));
        assert!(entry.clang_flags.contains(&isysroot_str));
        assert!(entry.clang_flags.contains(&"-nostdinc".to_owned()));
        assert!(
            entry
                .clang_flags
                .contains(&format!("-Wa,-I{},-march=rv64gcv", exp_asm_inc.display())),
            "flags: {:?}",
            entry.clang_flags
        );
        assert!(!entry.clang_flags.iter().any(|f| f == "-O2"));
        assert!(
            !entry
                .clang_flags
                .iter()
                .any(|f| f.contains("--fatal-warnings"))
        );
    }

    #[test]
    fn preprocessor_mode_respects_suffix_and_x_flag_overrides() {
        assert!(is_preprocessed_by_suffix(Path::new("boot.S")));
        assert!(is_preprocessed_by_suffix(Path::new("boot.sx")));
        assert!(is_preprocessed_by_suffix(Path::new("defs.h")));
        assert!(!is_preprocessed_by_suffix(Path::new("boot.s")));
        assert!(!is_preprocessed_by_suffix(Path::new("inc.inc")));

        let tmp =
            std::env::temp_dir().join(format!("riscv_asm_lsp_pre_ctx_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let dot_s = tmp.join("plain.s");
        let dot_big_s = tmp.join("raw.S");
        fs::write(&dot_s, "nop\n").unwrap();
        fs::write(&dot_big_s, "nop\n").unwrap();
        fs::write(
            tmp.join("compile_commands.json"),
            serde_json::json!([
                {
                    "directory": tmp.to_string_lossy(),
                    "arguments": ["clang", "-x", "assembler-with-cpp", "-c", dot_s.to_string_lossy()],
                    "file": dot_s.to_string_lossy()
                },
                {
                    "directory": tmp.to_string_lossy(),
                    "arguments": ["clang", "-xassembler", "-c", dot_big_s.to_string_lossy()],
                    "file": dot_big_s.to_string_lossy()
                }
            ])
            .to_string(),
        )
        .unwrap();

        let mut ctx = CompileContext::new(Some(tmp.clone()), Config::default());
        assert!(ctx.is_preprocessed_path(&dot_s));
        assert!(ctx.for_uri(&path_to_uri(&dot_s).unwrap()).preprocessed);
        assert!(!ctx.is_preprocessed_path(&dot_big_s));
        assert!(!ctx.for_uri(&path_to_uri(&dot_big_s).unwrap()).preprocessed);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn option_arch_compound_and_multiline_comment_and_hyphen_flags() {
        let mut spec = MarchSpec::parse("rv64gc");
        spec.apply_option_arch("rv64gc, -c, +zba");
        assert_eq!(spec.xlen, Xlen::Rv64);
        assert!(!spec.extensions.contains("c"));
        assert!(spec.extensions.contains("zba"));

        let src = "/*\n.option norvc\n*/\n    c.addi sp, -16\n";
        let (eff, opts) = effective_options_at_line(src, 3, &MarchSpec::parse("rv64gc"));
        assert!(eff.extensions.contains("c"));
        assert!(opts.is_empty());

        let args: Vec<String> = [
            "clang",
            "--target=x86_64-linux-gnu",
            "-include-pch",
            "pre.pch",
            "-isystem-after",
            "after_inc",
            "-I-",
            "-c",
            "x86.S",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let entry = extract_relevant_flags(Path::new("/proj"), &args);
        assert!(!is_riscv_target(entry.target.as_deref().unwrap()));
        assert!(entry.include_dirs.is_empty());
        assert!(!entry.clang_flags.iter().any(|f| f.contains("pch")));
        assert!(!entry.clang_flags.iter().any(|f| f.contains("after")));
        assert!(!entry.clang_flags.iter().any(|f| f == "-I-"));
    }

    #[test]
    fn march_implications_sequential_removal_vendor_and_compile_flags_multi_root() {
        let gc = MarchSpec::parse("rv64gc");
        assert!(gc.extensions.contains("m"));
        assert!(gc.extensions.contains("a"));
        assert!(gc.extensions.contains("f"));
        assert!(gc.extensions.contains("d"));
        assert!(gc.extensions.contains("c"));
        assert!(gc.extensions.contains("zca"));
        assert!(gc.extensions.contains("zcd"));
        assert!(gc.extensions.contains("zicsr"));
        assert!(gc.extensions.contains("zifencei"));

        let bv = MarchSpec::parse("rv64gcbv");
        for ext in ["zba", "zbb", "zbs", "zve64d", "zvl128b"] {
            assert!(bv.extensions.contains(ext), "missing {ext}");
        }

        let mut mod_spec = gc.clone();
        mod_spec.apply_option_arch("-m, -c");
        assert!(!mod_spec.extensions.contains("m"));
        assert!(!mod_spec.extensions.contains("c"));
        assert!(!mod_spec.extensions.contains("zca"));
        assert!(!mod_spec.extensions.contains("zcd"));
        assert!(mod_spec.to_llvm_mattr().split(',').any(|a| a == "-zca"));

        let mut no_rvc = gc;
        no_rvc.apply_rvc(false);
        assert!(!no_rvc.extensions.contains("c"));
        assert!(!no_rvc.extensions.contains("zca"));
        assert!(!no_rvc.extensions.contains("zcd"));

        let vendor = MarchSpec::parse("rv32im_xtheadba");
        assert_eq!(vendor.xlen, Xlen::Rv32);
        assert!(vendor.extensions.contains("m"));
        assert!(vendor.extensions.contains("xtheadba"));

        let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_multi_{}", std::process::id()));
        let root_a = tmp.join("root_a");
        let root_b = tmp.join("root_b");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(
            root_a.join("compile_flags.txt"),
            "-march=rv32imac\n-Iinc_a\n",
        )
        .unwrap();
        let file_a = root_a.join("a.S");
        let file_b = root_b.join("b.S");
        fs::write(&file_a, "nop\n").unwrap();
        fs::write(&file_b, "nop\n").unwrap();

        let mut ctx = CompileContext::new_multi(vec![root_a, root_b], Config::default());
        let ca = ctx.for_uri(&path_to_uri(&file_a).unwrap());
        assert_eq!(ca.march.xlen, Xlen::Rv32);
        assert!(ca.march.extensions.contains("c"));
        assert!(ca.include_dirs.iter().any(|p| p.ends_with("inc_a")));

        let cb = ctx.for_uri(&path_to_uri(&file_b).unwrap());
        assert_eq!(cb.march.xlen, Xlen::Rv64);

        let _ = fs::remove_dir_all(&tmp);
    }
}
