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

    // Item 5: AIA CSR (`stopi`), deprecated alias (`vpopc.m`), newly described `unimp`, new directives
    let h_stopi = hover_markdown(&mut client, &uri, 9, 15).expect("stopi");
    assert!(
        h_stopi.contains("0xdb0")
            && h_stopi.contains("S-mode")
            && !h_stopi.contains("No description"),
        "got: {h_stopi}"
    );
    let h_vpopc = hover_markdown(&mut client, &uri, 10, 6).expect("vpopc.m");
    assert!(
        h_vpopc.contains("Deprecated — prefer `vcpop.m`"),
        "got: {h_vpopc}"
    );
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

    // Multi-instruction encodings + CSR word hex encoding `0x10529073`
    let exp_uri: lsp_types::Uri = "file:///tmp/exp_test.S".parse().expect("uri");
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
