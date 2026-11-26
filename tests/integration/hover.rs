use crate::common::{LspClient, fixtures_dir, has_tool, hover_markdown, path_to_uri};
use serde_json::json;

#[test]
fn hover_covers_abi_registers_pseudos_csrs_and_directives() {
    let mut client = LspClient::start(None, None);
    let path = fixtures_dir().join("kernel_entry.S");
    let text = std::fs::read_to_string(&path).expect("read kernel_entry.S");
    let uri = path_to_uri(&path);
    client.open_doc(&uri, 1, &text);

    // Line 12: `    csrr a0, mhartid`
    let md = hover_markdown(&mut client, &uri, 12, 6).expect("hover csrr");
    assert!(md.contains("csrr"), "expected csrr in hover: {md}");

    let md = hover_markdown(&mut client, &uri, 12, 10).expect("hover a0");
    assert!(md.contains("x10"), "expected x10 in hover: {md}");

    let md = hover_markdown(&mut client, &uri, 12, 15).expect("hover mhartid");
    assert!(md.contains("0xf14"), "expected 0xf14 in hover: {md}");

    // Line 14: `    la sp, _stack_top`
    let md = hover_markdown(&mut client, &uri, 14, 5).expect("hover la");
    assert!(md.contains("la"), "expected la in hover: {md}");

    // Out-of-range hover returns null without crashing
    let oob = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 9999, "character": 9999 }
        }),
    );
    assert!(oob.error.is_none());

    client.shutdown_and_exit();
}

#[test]
fn hover_include_layouts_base_isa_and_march_disabled() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&kernel_uri, 1, &kernel_src);

    // `#include "regs.h"` shows resolved path
    let inc_hover = hover_markdown(&mut client, &kernel_uri, 0, 12).expect("#include hover");
    assert!(
        inc_hover.contains("regs.h"),
        "expected resolved regs.h path in hover: {inc_hover}"
    );

    // Base ISA (`addi` -> `I`, `ld` -> `RV64I`) + redesigned structured layout
    let addi_hover = hover_markdown(&mut client, &kernel_uri, 28, 4).expect("addi hover");
    assert!(
        addi_hover.contains("Instruction · Add immediate · `I` · RV32/RV64")
            && addi_hover.contains("```riscv\naddi rd, rs1, imm12")
            && addi_hover.contains("Operands:"),
        "expected structured addi hover: {addi_hover}"
    );
    assert!(!addi_hover.contains("Spec syntax"));
    assert!(!addi_hover.contains("*Source:"));

    let ld_hover = hover_markdown(&mut client, &kernel_uri, 30, 4).expect("ld hover");
    assert!(
        ld_hover.contains("`RV64I`"),
        "expected RV64I base: {ld_hover}"
    );
    assert!(
        !ld_hover.contains("Alias of: `ld`"),
        "unexpected self-alias in ld hover: {ld_hover}"
    );

    // Alias, pseudo, CSR (bullet list), register, directive, and project symbol
    let csrw_hover = hover_markdown(&mut client, &kernel_uri, 16, 4).expect("csrw hover");
    assert!(
        csrw_hover.contains("Alias of: `csrrw`"),
        "got: {csrw_hover}"
    );

    let la_pseudo = hover_markdown(&mut client, &kernel_uri, 14, 4).expect("la pseudo");
    assert!(la_pseudo.contains("Pseudo-instruction"), "got: {la_pseudo}");

    let stvec_hover = hover_markdown(&mut client, &kernel_uri, 16, 10).expect("stvec hover");
    assert!(stvec_hover.contains("0x105 · S-mode") && stvec_hover.contains("stvec"));
    assert!(stvec_hover.contains("- `MODE` [1:0]"));
    assert!(!stvec_hover.contains("| Bits |"));

    let sp_hover = hover_markdown(&mut client, &kernel_uri, 14, 7).expect("sp hover");
    assert!(
        sp_hover.contains("Register · Integer · callee-saved") && sp_hover.contains("sp (x2)"),
        "got: {sp_hover}"
    );

    let globl_hover = hover_markdown(&mut client, &kernel_uri, 10, 5).expect(".globl hover");
    assert!(
        globl_hover.starts_with("Directive · GNU / LLVM\n\n```riscv\n.globl"),
        "got: {globl_hover}"
    );
    assert!(!globl_hover.contains("*Source:"));

    let sym_hover = hover_markdown(&mut client, &kernel_uri, 2, 6).expect("KERNEL_BASE hover");
    assert!(
        sym_hover.contains("Constant (.equ)")
            && sym_hover.contains("kernel_entry.S:3")
            && sym_hover.contains("Value: `0x80200000`")
            && sym_hover.contains("```riscv"),
        "expected compact symbol hover: {sym_hover}"
    );

    // Item 7 + Item 4d: `-march` disabled instructions & `#define` in `.S` files
    let extra_uri = path_to_uri(&fixtures.join("virtual_march_and_define.S"));
    client.open_doc(
        &extra_uri,
        1,
        "#define LOCAL_VALUE 42\n    vadd.vv v1, v2, v3\n    add.uw a0, a1, a2\n    li t0, LOCAL_VALUE\n",
    );

    let vadd_hover = hover_markdown(&mut client, &extra_uri, 1, 5).expect("vadd.vv hover");
    assert!(vadd_hover.contains("V (or Zve32x)"), "got: {vadd_hover}");
    assert!(vadd_hover.contains("not enabled by `-march=rv64gc`"));
    assert!(!vadd_hover.contains("v|zve32x"));
    assert!(!vadd_hover.contains("expansion unavailable"));

    let adduw_hover = hover_markdown(&mut client, &extra_uri, 2, 5).expect("add.uw hover");
    assert!(adduw_hover.contains("Zba"));
    assert!(adduw_hover.contains("not enabled by `-march=rv64gc`"));

    let local_def_use = hover_markdown(&mut client, &extra_uri, 3, 12).expect("LOCAL_VALUE use");
    assert!(local_def_use.contains("LOCAL_VALUE") && local_def_use.contains("42"));

    let local_def_decl = hover_markdown(&mut client, &extra_uri, 0, 10).expect("LOCAL_VALUE def");
    assert!(local_def_decl.contains("LOCAL_VALUE") && local_def_decl.contains("42"));

    client.shutdown_and_exit();
}

#[test]
fn hover_covers_item5_7_8_9_and_16_improvements() {
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("hover_new_features_virtual.S"));
    let src = "\
.set CNT, 1
    li t0, CNT
.set CNT, CNT + 1
    li t1, CNT
VAL = 10
    li t2, VAL
    csrr a0, 0x105
    csrw 261, t0
    addi a0, zero, 0x105
    csrr t0, stopi
    vpopc.m v0, v1
    wfi
    unimp
    .ascii \"ok\"
    .purgem FOO
.macro M
lbl_\\@:
.endm
";
    client.open_doc(&uri, 1, src);

    // Item 8: `.set` reassignment shows active value and multi-assignment notice
    let h_before = hover_markdown(&mut client, &uri, 1, 12).expect("CNT before");
    assert!(h_before.contains("Symbol (.set)"), "got: {h_before}");
    assert!(h_before.contains("`1`"), "got: {h_before}");
    assert!(h_before.contains("Assigned 2 times"), "got: {h_before}");

    let h_after = hover_markdown(&mut client, &uri, 3, 12).expect("CNT after");
    assert!(h_after.contains("`CNT + 1`"), "got: {h_after}");

    let h_eq = hover_markdown(&mut client, &uri, 5, 12).expect("VAL =");
    assert!(
        h_eq.contains("Symbol (=)") && h_eq.contains("`10`"),
        "got: {h_eq}"
    );

    // Item 9: numeric CSR operands resolve when in CSR operand position, not in `addi`
    let h_csr_hex = hover_markdown(&mut client, &uri, 6, 15).expect("0x105");
    assert!(
        h_csr_hex.contains("stvec") && h_csr_hex.contains("0x105"),
        "got: {h_csr_hex}"
    );
    let h_csr_dec = hover_markdown(&mut client, &uri, 7, 10).expect("261");
    assert!(
        h_csr_dec.contains("stvec") && h_csr_dec.contains("0x105"),
        "got: {h_csr_dec}"
    );
    assert!(hover_markdown(&mut client, &uri, 8, 21).is_none());

    // Item 5: AIA CSR (`stopi`), newly described `unimp`, new directives
    let h_stopi = hover_markdown(&mut client, &uri, 9, 15).expect("stopi");
    assert!(
        h_stopi.contains("0xdb0")
            && h_stopi.contains("S-mode")
            && !h_stopi.contains("No description"),
        "got: {h_stopi}"
    );
    assert!(hover_markdown(&mut client, &uri, 10, 6).is_none());
    let h_unimp = hover_markdown(&mut client, &uri, 12, 6).expect("unimp");
    assert!(!h_unimp.contains("No description"), "got: {h_unimp}");
    assert!(hover_markdown(&mut client, &uri, 13, 6).is_some());
    assert!(hover_markdown(&mut client, &uri, 14, 6).is_some());

    // Item 16: fixed `wfi` description typo
    let h_wfi = hover_markdown(&mut client, &uri, 11, 5).expect("wfi");
    assert!(h_wfi.contains("Can cause the processor") && !h_wfi.contains("Can causes"));

    // Item 7: `\@` hover
    let h_counter = hover_markdown(&mut client, &uri, 16, 5).expect("\\@");
    assert!(
        h_counter.contains("Macro invocation counter"),
        "got: {h_counter}"
    );

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires clang and llvm-mc"]
fn hover_llvm_mc_and_clang_expansions() {
    assert!(
        has_tool("llvm-mc") && has_tool("clang"),
        "llvm-mc and clang must be on PATH for ignored tool tests"
    );

    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&kernel_uri, 1, &kernel_src);

    // Clean `la` expansion: filters `.Lpcrel_hi0:`, `.text`, `#   fixup`, uses `(relocated)`
    let la_hover = hover_markdown(&mut client, &kernel_uri, 14, 4).expect("la hover");
    let (_, exp_section) = la_hover
        .split_once("This line:")
        .expect("this-line section");
    assert!(exp_section.contains("auipc"), "exp: {exp_section}");
    assert!(exp_section.contains("(relocated)"), "exp: {exp_section}");
    assert!(
        !exp_section.contains("fixup"),
        "fixup leaked: {exp_section}"
    );
    assert!(
        !exp_section.contains("0bAAAA"),
        "placeholder leaked: {exp_section}"
    );
    assert!(
        !exp_section.contains(".Lpcrel_hi0:"),
        "label leaked: {exp_section}"
    );
    assert!(
        !exp_section.contains(".text"),
        "directive leaked: {exp_section}"
    );

    // Compressed hex instruction word `0x1141`
    let addi_exp = hover_markdown(&mut client, &kernel_uri, 28, 4).expect("addi exp");
    assert!(
        addi_exp.contains("This line: `c.addi sp, -16` → `0x1141`"),
        "got: {addi_exp}"
    );

    // `ret` after `.option` filters `.option` directives from expansion
    let opt_uri = path_to_uri(&fixtures.join("virtual_option_ret.S"));
    client.open_doc(&opt_uri, 1, ".option norvc\n.option rvc\n    ret\n");
    let ret_hover = hover_markdown(&mut client, &opt_uri, 2, 4).expect("ret hover");
    let (_, ret_exp) = ret_hover.split_once("This line:").expect("ret this-line");
    assert!(
        ret_exp.contains("c.jr") || ret_exp.contains("jalr"),
        "got: {ret_exp}"
    );
    assert!(!ret_exp.contains(".option"), "got: {ret_exp}");

    // `#define` constant expansion from `#include "regs.h"`
    let li_hover = hover_markdown(&mut client, &kernel_uri, 15, 4).expect("li hover");
    assert!(
        li_hover.contains("addi") || li_hover.contains("c.li"),
        "got: {li_hover}"
    );
    assert!(!li_hover.contains("expansion unavailable"));

    // `#define LOCAL_VALUE 42` in same `.S` file + unavailable fallback on unknown symbol
    let local_uri = path_to_uri(&fixtures.join("virtual_local_def_exp.S"));
    client.open_doc(
        &local_uri,
        1,
        "#define LOCAL_VALUE 42\n    li t0, LOCAL_VALUE\n    li a0, unknown_external_sym\n",
    );
    let local_li = hover_markdown(&mut client, &local_uri, 1, 5).expect("li LOCAL_VALUE");
    assert!(
        local_li.contains("addi") || local_li.contains("c.li"),
        "got: {local_li}"
    );
    assert!(!local_li.contains("expansion unavailable"));

    let unexp_hover = hover_markdown(&mut client, &local_uri, 2, 5).expect("unexp hover");
    assert!(
        unexp_hover.contains("expansion unavailable"),
        "got: {unexp_hover}"
    );

    // multi-instruction encodings + CSR word hex encoding `0x10529073`
    let exp_uri = crate::common::test_uri("exp_test.S");
    client.open_doc(
        &exp_uri,
        1,
        "    li a0, 0x12345678\n    csrw stvec, t0\n    mv a0, a1\n    la t0, sym\n",
    );
    for (line, expected) in [
        (0, "lui"),
        (1, "0x10529073"),
        (2, "c.mv"),
        (3, "(relocated)"),
    ] {
        let md = hover_markdown(&mut client, &exp_uri, line, 5).expect("hover");
        assert!(
            md.contains("This line:") && md.contains(expected),
            "line {line} missing {expected}: {md}"
        );
    }

    // `#ifdef` + multiline `#define`, register alias `#define TMP t0`, and cache isolation
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_hover_exp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("a.h"), "#define FN(x) ((x) + 1)\n").unwrap();
    std::fs::write(tmp.join("b.h"), "#define FN(x) ((x) + 100)\n").unwrap();
    let ifdef_s = tmp.join("ifdef.S");
    let fa_s = tmp.join("fa.S");
    let fb_s = tmp.join("fb.S");
    let ifdef_src = "\
#ifdef CONFIG_TEST
#define ADD_ONE(x) \\
    ((x) + 1)
    li a0, ADD_ONE(41)
#endif
#define TMP t0
    addi TMP, a0, 1
";
    let fa_src = "#include \"a.h\"\n    li a0, FN(1)\n";
    let fb_src = "#include \"b.h\"\n    li a0, FN(1)\n";
    std::fs::write(&ifdef_s, ifdef_src).unwrap();
    std::fs::write(&fa_s, fa_src).unwrap();
    std::fs::write(&fb_s, fb_src).unwrap();
    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy(),
            "arguments": ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-DCONFIG_TEST", "-c", ifdef_s.to_string_lossy()],
            "file": ifdef_s.to_string_lossy()
        }])
        .to_string(),
    )
    .unwrap();

    let mut client2 = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&path_to_uri(&tmp)),
    );
    let ifdef_uri = path_to_uri(&ifdef_s);
    let fa_uri = path_to_uri(&fa_s);
    let fb_uri = path_to_uri(&fb_s);
    client2.open_doc(&ifdef_uri, 1, ifdef_src);
    client2.open_doc(&fa_uri, 1, fa_src);
    client2.open_doc(&fb_uri, 1, fb_src);

    let h_ifdef = hover_markdown(&mut client2, &ifdef_uri, 3, 5).expect("ifdef hover");
    assert!(
        h_ifdef.contains("42") && !h_ifdef.contains("unavailable"),
        "got: {h_ifdef}"
    );
    let h_reg = hover_markdown(&mut client2, &ifdef_uri, 6, 5).expect("reg alias hover");
    assert!(h_reg.contains("addi t0, a0, 1"), "got: {h_reg}");

    let h_fa = hover_markdown(&mut client2, &fa_uri, 1, 5).expect("fa hover");
    let h_fb = hover_markdown(&mut client2, &fb_uri, 1, 5).expect("fb hover");
    assert!(h_fa.contains("c.li a0, 2"), "got: {h_fa}");
    assert!(h_fb.contains("addi a0, zero, 101"), "got: {h_fb}");

    client2.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires llvm-mc"]
fn hover_expands_cpp_macros_without_clang() {
    assert!(
        has_tool("llvm-mc"),
        "llvm-mc must be on PATH for ignored tool tests"
    );

    let fixtures = fixtures_dir();
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang",
            "diagnostics": { "enable": false }
        })),
        Some(&path_to_uri(&fixtures)),
    );
    client.open_doc(&kernel_uri, 1, &kernel_src);

    // Line 15 of kernel_entry.S uses `#define` from `#include "regs.h"` — resolved purely by B3!
    let li_hover = hover_markdown(&mut client, &kernel_uri, 15, 4).expect("li hover");
    assert!(
        li_hover.contains("This line:") && (li_hover.contains("addi") || li_hover.contains("c.li")),
        "expected built-in macro expansion without clang, got: {li_hover}"
    );
    assert!(!li_hover.contains("expansion unavailable"));

    // Function-like macro + `#ifdef` expansion without clang
    let fn_uri = path_to_uri(&fixtures.join("virtual_b3_hover.S"));
    client.open_doc(
        &fn_uri,
        1,
        "#define MAKE_IMM(x) ((x) + 10)\n#if __riscv_xlen == 64\n    li a0, MAKE_IMM(5)\n#endif\n",
    );
    let fn_hover = hover_markdown(&mut client, &fn_uri, 2, 5).expect("fn hover");
    assert!(
        fn_hover.contains("This line:") && fn_hover.contains("15"),
        "expected built-in function-like macro expansion without clang, got: {fn_hover}"
    );

    client.shutdown_and_exit();
}

#[test]
fn preprocessor_hover_distinguishes_s_suffix_and_x_override() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_hover_pp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    std::fs::write(tmp.join("defs.h"), "#define HDR_CONST 9\n").unwrap();
    std::fs::write(tmp.join("inc.inc"), ".equ INC_CONST, 7\n").unwrap();

    let pre_path = tmp.join("pre.S");
    let raw_path = tmp.join("raw.s");
    let forced_pre_path = tmp.join("forced_pre.s");
    let forced_raw_path = tmp.join("forced_raw.S");

    let pre_src = "\
#include \"defs.h\"
#define A 1
# if the value is zero
_start:
    li a0, A
    li a1, HDR_CONST
";
    let raw_src = "\
#include \"defs.h\"
    .include \"inc.inc\"
#define A 1
_start:
    li a0, A
    li a1, HDR_CONST
    li a2, INC_CONST
";
    std::fs::write(&pre_path, pre_src).unwrap();
    std::fs::write(&raw_path, raw_src).unwrap();
    std::fs::write(&forced_pre_path, pre_src).unwrap();
    std::fs::write(&forced_raw_path, pre_src).unwrap();

    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "-x", "assembler-with-cpp", "-c", forced_pre_path.to_string_lossy()],
                "file": forced_pre_path.to_string_lossy()
            },
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "-xassembler", "-c", forced_raw_path.to_string_lossy()],
                "file": forced_raw_path.to_string_lossy()
            }
        ])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let pre_uri = path_to_uri(&pre_path);
    let raw_uri = path_to_uri(&raw_path);
    let forced_pre_uri = path_to_uri(&forced_pre_path);
    let forced_raw_uri = path_to_uri(&forced_raw_path);

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&pre_uri, 1, pre_src);
    client.open_doc(&raw_uri, 1, raw_src);
    client.open_doc(&forced_pre_uri, 1, pre_src);
    client.open_doc(&forced_raw_uri, 1, pre_src);

    // `.S` preprocessed: directive hover, include path hover, `#define` symbol hover
    let h_inc_kw = hover_markdown(&mut client, &pre_uri, 0, 3).expect("#include keyword hover");
    assert!(
        h_inc_kw.contains("#include") && h_inc_kw.contains("not treated as a comment"),
        "got: {h_inc_kw}"
    );
    let h_inc_path = hover_markdown(&mut client, &pre_uri, 0, 12).expect("#include path hover");
    assert!(h_inc_path.contains("defs.h"), "got: {h_inc_path}");
    let h_def_kw = hover_markdown(&mut client, &pre_uri, 1, 3).expect("#define keyword hover");
    assert!(h_def_kw.contains("#define"), "got: {h_def_kw}");
    let h_if_kw = hover_markdown(&mut client, &pre_uri, 2, 2).expect("#if keyword hover");
    assert!(h_if_kw.contains("#if"), "got: {h_if_kw}");
    let h_a = hover_markdown(&mut client, &pre_uri, 4, 11).expect("A hover");
    assert!(h_a.contains("#define") && h_a.contains("1"), "got: {h_a}");
    let h_hdr = hover_markdown(&mut client, &pre_uri, 5, 14).expect("HDR_CONST hover");
    assert!(
        h_hdr.contains("#define") && h_hdr.contains("9"),
        "got: {h_hdr}"
    );

    // `.s` not preprocessed: `#include` and `#define` are comments, `.include` still resolves
    assert!(hover_markdown(&mut client, &raw_uri, 0, 3).is_none());
    assert!(hover_markdown(&mut client, &raw_uri, 0, 12).is_none());
    assert!(hover_markdown(&mut client, &raw_uri, 2, 3).is_none());
    assert!(hover_markdown(&mut client, &raw_uri, 2, 8).is_none());
    assert!(hover_markdown(&mut client, &raw_uri, 4, 11).is_none());
    assert!(hover_markdown(&mut client, &raw_uri, 5, 14).is_none());
    let h_asm_inc = hover_markdown(&mut client, &raw_uri, 1, 16).expect(".include path hover");
    assert!(h_asm_inc.contains("inc.inc"), "got: {h_asm_inc}");
    let h_inc_sym = hover_markdown(&mut client, &raw_uri, 6, 14).expect("INC_CONST hover");
    assert!(h_inc_sym.contains("7"), "got: {h_inc_sym}");

    // `.s` with `-x assembler-with-cpp` acts as preprocessed
    let h_forced_a = hover_markdown(&mut client, &forced_pre_uri, 4, 11).expect("forced .s A");
    assert!(h_forced_a.contains("#define") && h_forced_a.contains("1"));
    let h_forced_kw =
        hover_markdown(&mut client, &forced_pre_uri, 2, 2).expect("forced .s #if hover");
    assert!(h_forced_kw.contains("#if"));

    // `.S` with `-x assembler` acts as non-preprocessed
    assert!(hover_markdown(&mut client, &forced_raw_uri, 0, 3).is_none());
    assert!(hover_markdown(&mut client, &forced_raw_uri, 4, 11).is_none());

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn hover_formatting_csr_shadowing_multiline_block_and_decoded_filenames() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_hover_misc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("sub.inc"), "nop\n").unwrap();
    std::fs::write(tmp.join("sys.h"), "#define SYS 1\n").unwrap();

    let space_file = tmp.join("my file.S");
    let src = "\
    .include \"sub.inc\"
#include <sys.h>
/*
.option norvc
#define GHOST 99
*/ addi sp, sp, -16
    c.addi sp, -16
    csrr a0, mrmcfg
time:
    ret
_main:
    call time
    csrr a0, time
";
    std::fs::write(&space_file, src).unwrap();

    let root_uri = path_to_uri(&tmp);
    let uri = path_to_uri(&space_file);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&uri, 1, src);

    let h_asm_inc = hover_markdown(&mut client, &uri, 0, 15).expect(".include hover");
    assert!(
        h_asm_inc.contains("```riscv\n.include \"sub.inc\"\n```"),
        "got: {h_asm_inc}"
    );
    let h_sys_inc = hover_markdown(&mut client, &uri, 1, 12).expect("#include <...> hover");
    assert!(
        h_sys_inc.contains("```c\n#include <sys.h>\n```"),
        "got: {h_sys_inc}"
    );

    // `*/ addi sp, sp, -16` still resolves instruction hover, and `.option norvc` inside `/* */` is ignored
    let h_after_close = hover_markdown(&mut client, &uri, 5, 5).expect("*/ addi hover");
    assert!(
        h_after_close.contains("Add immediate"),
        "got: {h_after_close}"
    );
    let h_caddi = hover_markdown(&mut client, &uri, 6, 6).expect("c.addi hover");
    assert!(
        !h_caddi.contains("not enabled"),
        "commented .option leaked: {h_caddi}"
    );

    // Spec-only CSR shows LLVM notice
    let h_spec_csr = hover_markdown(&mut client, &uri, 7, 15).expect("mrmcfg hover");
    assert!(
        h_spec_csr.contains("Not accepted by LLVM 23"),
        "got: {h_spec_csr}"
    );

    // `call time` resolves user label with decoded filename (`my file.S`), `csrr a0, time` resolves CSR
    let h_call_time = hover_markdown(&mut client, &uri, 11, 10).expect("call time hover");
    assert!(
        h_call_time.contains("Label · `my file.S:9`"),
        "got: {h_call_time}"
    );
    let h_csr_time = hover_markdown(&mut client, &uri, 12, 15).expect("csrr time hover");
    assert!(h_csr_time.contains("CSR ·"), "got: {h_csr_time}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn hover_covers_csr_xlen_march_isa_label_operand_keywords_and_unresolved_include() {
    let uri = crate::common::test_uri("hover_adv_rv32.S");
    let src32 = "\
#include \"missing_header_xyz.h\"
    csrr a0, ssp
    csrr a0, vtype
    csrr a0, mcycleh
";
    let mut client32 = LspClient::start(
        Some(json!({ "defaultMarch": "rv32gc", "diagnostics": { "enable": false } })),
        None,
    );
    client32.open_doc(&uri, 1, src32);

    let h_inc = hover_markdown(&mut client32, &uri, 0, 14).expect("unresolved include hover");
    assert!(
        h_inc.contains("missing_header_xyz.h") && h_inc.contains("not found"),
        "got: {h_inc}"
    );

    let h_ssp32 = hover_markdown(&mut client32, &uri, 1, 14).expect("ssp32 hover");
    assert!(
        h_ssp32.contains("[31:2]"),
        "expected RV32 bits in ssp hover, got: {h_ssp32}"
    );

    let h_vtype32 = hover_markdown(&mut client32, &uri, 2, 14).expect("vtype32 hover");
    assert!(
        h_vtype32.contains("Not active") || h_vtype32.contains("-march"),
        "expected -march warning on vtype without V, got: {h_vtype32}"
    );

    let h_mcycleh32 = hover_markdown(&mut client32, &uri, 3, 14).expect("mcycleh32 hover");
    assert!(
        !h_mcycleh32.contains("Not active under current"),
        "unexpected inactive warning on RV32 mcycleh: {h_mcycleh32}"
    );
    client32.shutdown_and_exit();

    let uri64 = crate::common::test_uri("hover_adv_rv64.S");
    let src64 = "\
    mret
    sfence.vma
    ld a0, 0(sp)
    csrr a0, mcycleh
    lui a0, %hi(sym)
    fadd.s ft0, ft1, ft2, rne
    fence iorw, iorw
    vadd.vv v1, v2, v3, v0.t
";
    let mut client64 = LspClient::start(
        Some(json!({ "defaultMarch": "rv64gcv", "diagnostics": { "enable": false } })),
        None,
    );
    client64.open_doc(&uri64, 1, src64);

    let h_mret = hover_markdown(&mut client64, &uri64, 0, 5).expect("mret hover");
    assert!(h_mret.contains("`Sm`"), "got: {h_mret}");

    let h_sfence = hover_markdown(&mut client64, &uri64, 1, 6).expect("sfence hover");
    assert!(h_sfence.contains("`S`"), "got: {h_sfence}");

    let h_ld = hover_markdown(&mut client64, &uri64, 2, 5).expect("ld hover");
    assert!(
        h_ld.contains("`RV64I`") && !h_ld.contains("Zilsd"),
        "got: {h_ld}"
    );

    let h_mcycleh64 = hover_markdown(&mut client64, &uri64, 3, 15).expect("mcycleh64 hover");
    assert!(
        h_mcycleh64.contains("RV32"),
        "expected RV32 notice on RV64 mcycleh, got: {h_mcycleh64}"
    );

    let h_hi = hover_markdown(&mut client64, &uri64, 4, 14).expect("%hi hover");
    assert!(h_hi.contains("%hi"), "got: {h_hi}");

    let h_rne = hover_markdown(&mut client64, &uri64, 5, 27).expect("rne hover");
    assert!(h_rne.to_ascii_lowercase().contains("round"), "got: {h_rne}");

    let h_iorw = hover_markdown(&mut client64, &uri64, 6, 11).expect("iorw hover");
    assert!(h_iorw.contains("iorw"), "got: {h_iorw}");

    let h_v0t = hover_markdown(&mut client64, &uri64, 7, 25).expect("v0.t hover");
    assert!(h_v0t.contains("v0.t"), "got: {h_v0t}");

    client64.shutdown_and_exit();
}

#[test]
fn hover_builtin_expansion_works_without_external_tools() {
    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang",
            "llvmMc": "/nonexistent/llvm-mc",
            "defaultMarch": "rv64gc",
            "diagnostics": { "enable": false }
        })),
        None,
    );
    let uri = crate::common::test_uri("builtin_exp_offline.S");
    let src = "\
#define IMM_VAL 0x12345678
_start:
    li a0, IMM_VAL
    csrw stvec, t0
    mv a0, a1
    addi sp, sp, -16
    la sp, _stack_top
";
    client.open_doc(&uri, 1, src);

    let h_li = hover_markdown(&mut client, &uri, 2, 5).expect("li hover");
    assert!(
        h_li.contains("0x12345537") && h_li.contains("0x67850513"),
        "expected built-in li encoding without llvm-mc, got: {h_li}"
    );

    let h_csrw = hover_markdown(&mut client, &uri, 3, 5).expect("csrw hover");
    assert!(h_csrw.contains("0x10529073"), "got: {h_csrw}");

    let h_mv = hover_markdown(&mut client, &uri, 4, 5).expect("mv hover");
    assert!(
        h_mv.contains("c.mv a0, a1") && h_mv.contains("0x852e"),
        "got: {h_mv}"
    );

    let h_addi = hover_markdown(&mut client, &uri, 5, 5).expect("addi hover");
    assert!(
        h_addi.contains("c.addi sp, -16") && h_addi.contains("0x1141"),
        "got: {h_addi}"
    );

    let h_la = hover_markdown(&mut client, &uri, 6, 5).expect("la hover");
    assert!(
        h_la.contains("auipc sp, %pcrel_hi(_stack_top)") && h_la.contains("(relocated)"),
        "got: {h_la}"
    );

    client.shutdown_and_exit();
}

#[test]
fn hover_avoids_mixed_operands_zfinx_long_csr_and_cut_amo_summary() {
    let mut client = LspClient::start(Some(json!({ "defaultMarch": "rv64gc_zabha_zacas" })), None);
    let uri = crate::common::test_uri("hover_polish.S");
    let src = "\
_start:
    sd a0, 8(sp)
    flw fa0, 0(sp)
    fadd.s fa0, fa1, fa2
    csrr a0, mstatus
    amocas.h.aqrl a0, a1, (a2)
";
    client.open_doc(&uri, 1, src);

    let sd_hover = hover_markdown(&mut client, &uri, 1, 5).expect("sd hover");
    assert!(
        !sd_hover.contains("`tmp`") && !sd_hover.contains("`addr`"),
        "sd hover mixed pseudo operands into Operands fact: {sd_hover}"
    );
    assert!(sd_hover.contains("Operands:"), "got: {sd_hover}");

    let flw_hover = hover_markdown(&mut client, &uri, 2, 5).expect("flw hover");
    assert!(
        !flw_hover.contains("`tmp`") && !flw_hover.contains("`addr`"),
        "flw hover mixed pseudo operands into Operands fact: {flw_hover}"
    );
    assert!(flw_hover.contains("FP32 reg"), "got: {flw_hover}");

    let fadd_hover = hover_markdown(&mut client, &uri, 3, 6).expect("fadd.s hover");
    assert!(
        fadd_hover.contains("FP32 reg") && !fadd_hover.contains("GPR"),
        "fadd.s hover showed Zfinx operand description instead of F: {fadd_hover}"
    );

    let mstatus_hover = hover_markdown(&mut client, &uri, 4, 15).expect("mstatus hover");
    let field_lines: Vec<&str> = mstatus_hover
        .lines()
        .filter(|l| l.trim_start().starts_with("- `"))
        .collect();
    assert!(
        !field_lines.is_empty() && field_lines.len() <= 8,
        "mstatus hover should bound shown fields: {mstatus_hover}"
    );
    for field_line in field_lines {
        assert!(
            field_line.chars().count() <= 180,
            "mstatus field line unexpectedly long: {field_line}"
        );
    }

    let amo_hover = hover_markdown(&mut client, &uri, 5, 6).expect("amocas.h.aqrl hover");
    assert!(
        !amo_hover.contains("…"),
        "amocas.h.aqrl summary unexpectedly truncated: {amo_hover}"
    );

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires llvm-mc"]
fn builtin_expansion_and_encoding_match_llvm_mc() {
    assert!(
        has_tool("llvm-mc"),
        "llvm-mc must be on PATH for ignored tool tests"
    );

    use riscv_asm_lsp::context::MarchSpec;
    use riscv_asm_lsp::data::Database;
    use riscv_asm_lsp::hover::expand_and_encode;
    use std::collections::HashMap;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let db = Database::load().expect("db");

    fn mc_encode(triple: &str, mattr: &str, stmt: &str) -> Vec<(String, String)> {
        let mut args = vec![format!("-triple={triple}")];
        if !mattr.is_empty() {
            args.push(format!("-mattr={mattr}"));
        }
        args.extend([
            "-show-encoding".to_owned(),
            "-M".to_owned(),
            "no-aliases".to_owned(),
        ]);
        let mut child = Command::new("llvm-mc")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn llvm-mc");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{stmt}\n").as_bytes());
        }
        let out = child.wait_with_output().expect("wait llvm-mc");
        assert!(
            out.status.success(),
            "llvm-mc failed on `{stmt}` with {triple} {mattr}"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut items = Vec::new();
        for raw in stdout.lines() {
            let trimmed = raw.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('.')
                || trimmed.starts_with('#')
                || trimmed.ends_with(':')
            {
                continue;
            }
            if let Some((asm_part, enc_part)) = trimmed.split_once("# encoding:") {
                let asm = asm_part.split_whitespace().collect::<Vec<_>>().join(" ");
                let open = enc_part.find('[').unwrap();
                let close = enc_part[open + 1..].find(']').unwrap();
                let inner = enc_part[open + 1..open + 1 + close].trim();
                let bytes: Option<Vec<u8>> = inner
                    .split(',')
                    .map(|t| {
                        t.trim()
                            .strip_prefix("0x")
                            .and_then(|h| u8::from_str_radix(h, 16).ok())
                    })
                    .collect();
                let enc = match bytes {
                    Some(b) if b.len() == 2 => {
                        format!("0x{:04x}", u16::from_le_bytes([b[0], b[1]]))
                    }
                    Some(b) if b.len() == 4 => {
                        format!("0x{:08x}", u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    }
                    _ => "(relocated)".to_owned(),
                };
                items.push((asm, enc));
            }
        }
        items
    }

    let inst_cases: &[(&str, &str)] = &[
        ("rv64gc", "addi a0, a1, 42"),
        ("rv64gc", "addi sp, sp, -16"),
        ("rv64gc", "addi sp, sp, -48"),
        ("rv64gc", "addi s0, sp, 16"),
        ("rv64gc", "add a0, a1, a2"),
        ("rv64gc", "add a0, a0, a2"),
        ("rv64gc", "sub a0, a0, a1"),
        ("rv64gc", "ld a0, 16(sp)"),
        ("rv64gc", "ld s0, 8(s1)"),
        ("rv64gc", "sd ra, 24(sp)"),
        ("rv64gc", "lw a0, 4(sp)"),
        ("rv64gc", "sw a0, 4(s1)"),
        ("rv64gc", "lui a0, 0x12345"),
        ("rv64gc", "lui a0, 7"),
        ("rv64gc", "auipc a0, 12"),
        ("rv64gc", "csrw stvec, t0"),
        ("rv64gc", "csrr a0, mstatus"),
        ("rv64gc", "csrrwi a0, mscratch, 5"),
        ("rv64gc", "mv a0, a1"),
        ("rv64gc", "nop"),
        ("rv64gc", "ret"),
        ("rv64gc", "not a0, a1"),
        ("rv64gc", "neg a0, a1"),
        ("rv64gc", "seqz a0, a1"),
        ("rv64gc", "snez a0, a1"),
        ("rv64gc", "jr a0"),
        ("rv64gc", "jalr a0"),
        ("rv64gc", "beq a0, a1, 16"),
        ("rv64gc", "beq a0, a1, -16"),
        ("rv64gc", "beqz a0, 16"),
        ("rv64gc", "jal x0, 32"),
        ("rv64gc", "j 32"),
        ("rv64gc", "j -8"),
        ("rv64gc", "ebreak"),
        ("rv64gc", "ecall"),
        ("rv64gc", "fence iorw, ow"),
        ("rv64gc", "lr.w a0, (a1)"),
        ("rv64gc", "amoswap.w a0, a1, (a2)"),
        ("rv64gc", "fadd.s fa0, fa1, fa2, rne"),
        ("rv64gc", "fadd.s fa0, fa1, fa2"),
        ("rv64gc", "fld fa0, 16(sp)"),
        ("rv32imac", "addi a0, a1, -1"),
        ("rv32imac", "jal ra, 16"),
        ("rv32imac", "lw a0, 8(s1)"),
        ("rv64imad", "addi sp, sp, -16"),
        ("rv64imad", "mv a0, a1"),
        ("rv64imad", "ret"),
        ("rv64gcv", "vadd.vv v1, v2, v3"),
        ("rv64gcv", "vadd.vv v1, v2, v3, v0.t"),
    ];

    let empty = HashMap::new();
    for &(march_str, stmt) in inst_cases {
        let spec = MarchSpec::parse(march_str);
        let mc = mc_encode(spec.llvm_triple(), &spec.to_llvm_mattr(), stmt);
        let ours = expand_and_encode(stmt, &spec, &empty, &db)
            .unwrap_or_else(|| panic!("builtin failed to expand `{stmt}` under {march_str}"));
        let our_enc: Vec<&str> = ours
            .iter()
            .map(|e| e.encoding.as_deref().expect("encoding present"))
            .collect();
        let mc_enc: Vec<&str> = mc.iter().map(|(_, e)| e.as_str()).collect();
        assert_eq!(
            our_enc, mc_enc,
            "encoding mismatch for `{stmt}` under {march_str}: ours={ours:?} vs mc={mc:?}"
        );
    }

    // Wide matrix of `li rd, val` constants across RV32/RV64 and Zba/Zbb/Zbs/no-C combinations
    let li_vals: &[&str] = &[
        "0",
        "1",
        "-1",
        "31",
        "32",
        "-32",
        "-33",
        "2047",
        "2048",
        "-2048",
        "-2049",
        "0x12345",
        "0x12345678",
        "0x7ffff800",
        "0x7fffffff",
        "-0x80000000",
        "0x80000000",
        "0xffffffff",
        "0x100000000",
        "0x100000001",
        "0xdeadbeefcafebabe",
        "0x10000000000",
        "-0x1000001",
        "0x369d0369",
        "0x12345678000",
        "-0x123456789abc",
        "0x0000ffff0000ffff",
        "0x7fffffffffffffff",
        "-0x8000000000000000",
    ];
    for march_str in [
        "rv64gc",
        "rv64imad",
        "rv64gc_zba",
        "rv64gc_zbb",
        "rv64gc_zbs",
        "rv64gcb_zbs",
        "rv32imac",
    ] {
        let spec = MarchSpec::parse(march_str);
        for val_str in li_vals {
            if spec.xlen == riscv_asm_lsp::data::Xlen::Rv32
                && matches!(
                    *val_str,
                    "0x100000000"
                        | "0x100000001"
                        | "0xdeadbeefcafebabe"
                        | "0x10000000000"
                        | "0x12345678000"
                        | "-0x123456789abc"
                        | "0x0000ffff0000ffff"
                        | "0x7fffffffffffffff"
                        | "-0x8000000000000000"
                )
            {
                continue;
            }
            let stmt = format!("li a0, {val_str}");
            let mc = mc_encode(spec.llvm_triple(), &spec.to_llvm_mattr(), &stmt);
            let ours = expand_and_encode(&stmt, &spec, &empty, &db)
                .unwrap_or_else(|| panic!("builtin failed to expand `{stmt}` under {march_str}"));
            let our_enc: Vec<&str> = ours
                .iter()
                .map(|e| e.encoding.as_deref().expect("encoding present"))
                .collect();
            let mc_enc: Vec<&str> = mc.iter().map(|(_, e)| e.as_str()).collect();
            assert_eq!(
                our_enc, mc_enc,
                "li encoding mismatch for `{stmt}` under {march_str}: ours={ours:?} vs mc={mc:?}"
            );
        }
    }
}
