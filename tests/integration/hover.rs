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

    // Base ISA (`addi` -> `I`, `ld` -> `RV64I`) + E1 structured compact layout
    let addi_hover = hover_markdown(&mut client, &kernel_uri, 28, 4).expect("addi hover");
    assert!(
        addi_hover.starts_with("```riscv\naddi rd, rs1, imm12"),
        "expected syntax fence first: {addi_hover}"
    );
    assert!(
        addi_hover.contains("**Add immediate**") && addi_hover.contains("`I` · RV32/RV64"),
        "expected base ISA `I` and RV32/RV64: {addi_hover}"
    );
    assert!(!addi_hover.contains("Spec syntax"));
    assert!(!addi_hover.contains("*Source:"));

    let ld_hover = hover_markdown(&mut client, &kernel_uri, 30, 4).expect("ld hover");
    assert!(
        ld_hover.contains("`RV64I`"),
        "expected RV64I base: {ld_hover}"
    );
    assert!(
        !ld_hover.contains("alias of `ld`"),
        "unexpected self-alias in ld hover: {ld_hover}"
    );

    // Alias, pseudo, CSR (bullet list), register (1-line), directive, and project symbol
    let csrw_hover = hover_markdown(&mut client, &kernel_uri, 16, 4).expect("csrw hover");
    assert!(csrw_hover.contains("alias of `csrrw`"), "got: {csrw_hover}");

    let la_pseudo = hover_markdown(&mut client, &kernel_uri, 14, 4).expect("la pseudo");
    assert!(la_pseudo.contains("pseudo-instruction"), "got: {la_pseudo}");

    let stvec_hover = hover_markdown(&mut client, &kernel_uri, 16, 10).expect("stvec hover");
    assert!(stvec_hover.contains("`stvec` · 0x105 · S-mode"));
    assert!(stvec_hover.contains("- `MODE` [1:0]"));
    assert!(!stvec_hover.contains("| Bits |"));

    let sp_hover = hover_markdown(&mut client, &kernel_uri, 14, 7).expect("sp hover");
    assert!(
        !sp_hover.contains('\n'),
        "register hover must be 1 line: {sp_hover}"
    );
    assert!(sp_hover.contains("`sp` (x2)") && sp_hover.contains("callee-saved"));

    let globl_hover = hover_markdown(&mut client, &kernel_uri, 10, 5).expect(".globl hover");
    assert!(globl_hover.starts_with("```riscv\n.globl"));
    assert!(!globl_hover.contains("*Source:"));

    let sym_hover = hover_markdown(&mut client, &kernel_uri, 2, 6).expect("KERNEL_BASE hover");
    assert!(
        sym_hover.contains("constant (.equ)")
            && sym_hover.contains("kernel_entry.S:3")
            && sym_hover.contains("0x80200000")
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
        .split_once("**This line:**")
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
        addi_exp.contains("**This line:** `c.addi sp, -16` · `0x1141`"),
        "got: {addi_exp}"
    );

    // `ret` after `.option` filters `.option` directives from expansion
    let opt_uri = path_to_uri(&fixtures.join("virtual_option_ret.S"));
    client.open_doc(&opt_uri, 1, ".option norvc\n.option rvc\n    ret\n");
    let ret_hover = hover_markdown(&mut client, &opt_uri, 2, 4).expect("ret hover");
    let (_, ret_exp) = ret_hover
        .split_once("**This line:**")
        .expect("ret this-line");
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
            md.contains("**This line:**") && md.contains(expected),
            "line {line} missing {expected}: {md}"
        );
    }

    client.shutdown_and_exit();
}
