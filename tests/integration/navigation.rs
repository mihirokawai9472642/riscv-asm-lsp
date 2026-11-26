use crate::common::{LspClient, fixtures_dir, path_to_uri};
#[cfg(unix)]
use crate::common::{lexical_path_to_uri, workspace_symbol_names};
use serde_json::json;

#[test]
fn navigation_symbols_and_rename_across_files() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let mut client = LspClient::start(None, Some(&root_uri));

    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_text = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let other_uri = path_to_uri(&fixtures.join("other.S"));
    let regs_uri = path_to_uri(&fixtures.join("regs.h"));

    client.open_doc(&kernel_uri, 1, &kernel_text);

    // Go to definition on `#include "regs.h"` -> `regs.h`
    let def_inc = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 0, "character": 12 }
        }),
    );
    let def_inc_str = serde_json::to_string(&def_inc.result.expect("def")).expect("json");
    assert!(def_inc_str.contains("regs.h"));

    // Numeric labels: `1f` on line 13 -> `1:` on line 20; `1b` on line 22 -> `1:` on line 20
    let def_1f = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 13, "character": 14 }
        }),
    );
    assert_eq!(
        def_1f.result.as_ref().unwrap()["range"]["start"]["line"],
        20
    );

    let def_1b = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 22, "character": 7 }
        }),
    );
    assert_eq!(
        def_1b.result.as_ref().unwrap()["range"]["start"]["line"],
        20
    );

    // Hover on `1f` shows target line and label
    let hover_1f = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 13, "character": 14 }
        }),
    );
    assert!(
        hover_1f.result.as_ref().unwrap()["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .contains("21")
    );

    // `#define` in included header: `MSTATUS_MIE` on line 15 -> `regs.h`
    let def_macro_hdr = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 15, "character": 14 }
        }),
    );
    assert_eq!(
        def_macro_hdr.result.as_ref().unwrap()["uri"].as_str(),
        Some(regs_uri.as_str())
    );

    // `.macro`: `SAVE_REG` on line 18 -> line 5; `\reg` on line 6 -> param on line 5
    let def_macro = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 18, "character": 6 }
        }),
    );
    assert_eq!(
        def_macro.result.as_ref().unwrap()["range"]["start"]["line"],
        5
    );

    let def_param = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 6, "character": 9 }
        }),
    );
    assert_eq!(
        def_param.result.as_ref().unwrap()["range"]["start"]["line"],
        5
    );

    // Cross-file label: `call kernel_main` on line 19 -> `other.S`
    let def_cross = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 19, "character": 12 }
        }),
    );
    assert_eq!(
        def_cross.result.as_ref().unwrap()["uri"].as_str(),
        Some(other_uri.as_str())
    );

    // References on `1:` (line 20) includes `1:`, `1f`, `1b`
    let refs_1 = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 20, "character": 0 },
            "context": { "includeDeclaration": true }
        }),
    );
    assert_eq!(refs_1.result.as_ref().unwrap().as_array().unwrap().len(), 3);

    // Rename refuses numeric labels (`1f`) and accepts ordinary labels (`trap_vector`)
    let prep_num = client.send_request(
        "textDocument/prepareRename",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 13, "character": 14 }
        }),
    );
    assert!(prep_num.result.is_none() || prep_num.result == Some(serde_json::Value::Null));

    let rename_trap = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 26, "character": 2 },
            "newName": "trap_handler"
        }),
    );
    let changes = rename_trap.result.as_ref().unwrap()["changes"]
        .as_object()
        .unwrap();
    assert!(changes.contains_key(kernel_uri.as_str()));

    // Document symbols list `_start` with label range ending before `trap_vector` (line 26)
    let doc_syms = client.send_request(
        "textDocument/documentSymbol",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() }
        }),
    );
    let syms = doc_syms.result.as_ref().unwrap().as_array().unwrap();
    let start_sym = syms
        .iter()
        .find(|s| s["name"] == "_start")
        .expect("_start sym");
    assert!(start_sym["range"]["end"]["line"].as_u64().unwrap() < 26);
    assert_eq!(start_sym["children"].as_array().unwrap()[0]["name"], "1:");

    // `.Llocal_helper` in `other.S` resolves inside `other.S`
    let other_text = std::fs::read_to_string(fixtures.join("other.S")).expect("read other.S");
    client.open_doc(&other_uri, 1, &other_text);
    let def_local = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": other_uri.as_str() },
            "position": { "line": 5, "character": 14 }
        }),
    );
    assert_eq!(
        def_local.result.as_ref().unwrap()["uri"].as_str(),
        Some(other_uri.as_str())
    );
    assert_eq!(
        def_local.result.as_ref().unwrap()["range"]["start"]["line"],
        11
    );

    // `.equ KERNEL_BASE` definition
    let def_equ = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 2, "character": 6 }
        }),
    );
    assert_eq!(
        def_equ.result.as_ref().unwrap()["range"]["start"]["line"],
        2
    );

    // Workspace symbols find `kernel_main` from `other.S`
    let ws_syms = client.send_request(
        "workspace/symbol",
        json!({
            "query": "kernel_main"
        }),
    );
    assert!(
        ws_syms
            .result
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "kernel_main")
    );

    client.shutdown_and_exit();
}

#[test]
fn define_in_assembly_file_navigation_and_references() {
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("define_in_asm_virtual.S"));
    client.open_doc(
        &uri,
        1,
        "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n",
    );

    let def = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 14 }
        }),
    );
    assert_eq!(def.result.as_ref().unwrap()["range"]["start"]["line"], 0);

    let refs = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 14 },
            "context": { "includeDeclaration": true }
        }),
    );
    let arr = refs.result.as_ref().unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 2);

    client.shutdown_and_exit();
}

#[test]
fn rename_rejects_invalid_names_builtins_and_scope_collisions() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_ren_val_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let a_path = tmp.join("a.S");
    let b_path = tmp.join("b.S");
    let a_src = "\
#define DEF_A 1
.equ CONST_A, 2
start:
    j .Llocal
    ret
.Llocal:
    nop
data:
    .word 0
";
    let b_src = "other_global:\n    ret\n";
    std::fs::write(&a_path, a_src).unwrap();
    std::fs::write(&b_path, b_src).unwrap();

    let root_uri = path_to_uri(&tmp);
    let a_uri = path_to_uri(&a_path);
    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&a_uri, 1, a_src);

    // Renaming `start` (line 2, col 1) must reject invalid identifiers, registers, CSRs,
    // directives, same-file labels (`data`), and cross-file global labels (`other_global`).
    for bad in [
        "1bad",
        "bad name",
        "",
        "a0",
        "x5",
        "fa0",
        "stvec",
        ".globl",
        "data",
        "other_global",
    ] {
        let resp = client.send_request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": a_uri.as_str() },
                "position": { "line": 2, "character": 1 },
                "newName": bad
            }),
        );
        assert!(
            resp.error.is_some(),
            "expected rename `start` -> `{bad}` to error, got {:?}",
            resp.result
        );
    }

    // Renaming `.Llocal` (line 5, col 2) to same-file `data` must fail.
    let local_col = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": a_uri.as_str() },
            "position": { "line": 5, "character": 2 },
            "newName": "data"
        }),
    );
    assert!(local_col.error.is_some());

    // Renaming `CONST_A` (line 1, col 6) to `#define DEF_A` in same file must fail.
    let const_col = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": a_uri.as_str() },
            "position": { "line": 1, "character": 6 },
            "newName": "DEF_A"
        }),
    );
    assert!(const_col.error.is_some());

    // Valid unique name still succeeds.
    let ok = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": a_uri.as_str() },
            "position": { "line": 2, "character": 1 },
            "newName": "entry_point"
        }),
    );
    assert!(ok.error.is_none());
    assert!(ok.result.as_ref().unwrap()["changes"][a_uri.as_str()].is_array());

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn irp_and_irpc_navigation_references_and_rename() {
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("irp_nav_virtual.S"));
    let src = "\
.irp r, a0, a1
    mv \\r, zero
.endr
.irpc c, 01
    .byte \\c
.endr
";
    client.open_doc(&uri, 1, src);

    // Definition from `\r` (line 1, col 8) -> `r` on line 0
    let def = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 8 }
        }),
    );
    assert_eq!(def.result.as_ref().unwrap()["range"]["start"]["line"], 0);

    // References on `.irp r` -> decl + `\r`
    let refs = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 8 },
            "context": { "includeDeclaration": true }
        }),
    );
    assert_eq!(refs.result.as_ref().unwrap().as_array().unwrap().len(), 2);

    // Rename `r` -> `reg_v` preserves `\` on line 1
    let ren = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 8 },
            "newName": "reg_v"
        }),
    );
    assert!(ren.error.is_none());
    let edits = ren.result.as_ref().unwrap()["changes"][uri.as_str()]
        .as_array()
        .unwrap();
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|e| e["newText"] == "reg_v"));
    let ref_edit = edits
        .iter()
        .find(|e| e["range"]["start"]["line"] == 1)
        .unwrap();
    assert_eq!(ref_edit["range"]["start"]["character"], 8); // after `\`

    // Definition from `\c` in `.irpc` (line 4, col 11) -> line 3
    let def_irpc = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 4, "character": 11 }
        }),
    );
    assert_eq!(
        def_irpc.result.as_ref().unwrap()["range"]["start"]["line"],
        3
    );

    client.shutdown_and_exit();
}

#[cfg(unix)]
#[test]
fn symlinked_workspace_preserves_client_uris_in_navigation_and_symbols() {
    let base = std::env::temp_dir().join(format!("riscv_asm_lsp_sym_ws_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let real_dir = base.join("ws dir+日本");
    let link_dir = base.join("link");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(real_dir.join("defs.h"), "#define SYM_CONST 7\n").unwrap();
    std::fs::write(real_dir.join("b.S"), "sym_helper:\n    ret\n").unwrap();
    let a_src = "#include \"defs.h\"\n_start:\n    li a0, SYM_CONST\n    call sym_helper\n";
    std::fs::write(real_dir.join("a.S"), a_src).unwrap();
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    let link_root_uri = lexical_path_to_uri(&link_dir);
    let link_a_uri = lexical_path_to_uri(&link_dir.join("a.S"));
    let link_b_uri = lexical_path_to_uri(&link_dir.join("b.S"));
    let link_h_uri = lexical_path_to_uri(&link_dir.join("defs.h"));

    let mut client = LspClient::start(None, Some(&link_root_uri));
    client.open_doc(&link_a_uri, 1, a_src);

    // `#include "defs.h"` -> symlink URI
    let def_inc = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": link_a_uri.as_str() },
            "position": { "line": 0, "character": 12 }
        }),
    );
    assert_eq!(
        def_inc.result.as_ref().unwrap()["uri"],
        json!(link_h_uri.as_str())
    );

    // `SYM_CONST` -> symlink URI
    let def_hdr = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": link_a_uri.as_str() },
            "position": { "line": 2, "character": 12 }
        }),
    );
    assert_eq!(
        def_hdr.result.as_ref().unwrap()["uri"],
        json!(link_h_uri.as_str())
    );

    // `sym_helper` -> symlink `b.S` URI
    let def_cross = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": link_a_uri.as_str() },
            "position": { "line": 3, "character": 10 }
        }),
    );
    assert_eq!(
        def_cross.result.as_ref().unwrap()["uri"],
        json!(link_b_uri.as_str())
    );

    // Rename `sym_helper` returns edits keyed with symlink URIs
    let ren = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": link_a_uri.as_str() },
            "position": { "line": 3, "character": 10 },
            "newName": "sym_helper_v2"
        }),
    );
    let changes = ren.result.as_ref().unwrap()["changes"].as_object().unwrap();
    assert!(
        changes.contains_key(link_a_uri.as_str()),
        "changes: {changes:?}"
    );
    assert!(
        changes.contains_key(link_b_uri.as_str()),
        "changes: {changes:?}"
    );

    assert!(
        workspace_symbol_names(&mut client, "sym_helper")
            .iter()
            .any(|s| s == "sym_helper")
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn preprocessor_navigation_respects_suffix_and_x_override() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_nav_pp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let defs_h = tmp.join("defs.h");
    let inc_inc = tmp.join("inc.inc");
    let pre_s = tmp.join("pre.S");
    let raw_s = tmp.join("raw.s");
    let forced_s = tmp.join("forced.s");

    std::fs::write(&defs_h, "#define HDR_CONST 9\n").unwrap();
    std::fs::write(&inc_inc, ".equ INC_CONST, 7\n").unwrap();

    let pre_src = "\
#include \"defs.h\"
#define A 1
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
    std::fs::write(&pre_s, pre_src).unwrap();
    std::fs::write(&raw_s, raw_src).unwrap();
    std::fs::write(&forced_s, pre_src).unwrap();
    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy(),
            "arguments": ["clang", "-x", "assembler-with-cpp", "-c", forced_s.to_string_lossy()],
            "file": forced_s.to_string_lossy()
        }])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let defs_uri = path_to_uri(&defs_h);
    let inc_uri = path_to_uri(&inc_inc);
    let pre_uri = path_to_uri(&pre_s);
    let raw_uri = path_to_uri(&raw_s);
    let forced_uri = path_to_uri(&forced_s);

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&pre_uri, 1, pre_src);
    client.open_doc(&raw_uri, 1, raw_src);
    client.open_doc(&forced_uri, 1, pre_src);

    // `.S` resolves `#include`, local `#define A`, and header `#define HDR_CONST`
    let def_pre_inc = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": pre_uri.as_str() },
            "position": { "line": 0, "character": 12 }
        }),
    );
    assert_eq!(
        def_pre_inc.result.as_ref().unwrap()["uri"],
        json!(defs_uri.as_str())
    );
    let def_pre_a = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": pre_uri.as_str() },
            "position": { "line": 3, "character": 11 }
        }),
    );
    assert_eq!(
        def_pre_a.result.as_ref().unwrap()["range"]["start"]["line"],
        1
    );
    let def_pre_hdr = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": pre_uri.as_str() },
            "position": { "line": 4, "character": 14 }
        }),
    );
    assert_eq!(
        def_pre_hdr.result.as_ref().unwrap()["uri"],
        json!(defs_uri.as_str())
    );

    // `.s` ignores `#include` and `#define`, but keeps `.include`
    for (line, ch) in [(0, 12), (4, 11), (5, 14)] {
        let resp = client.send_request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": raw_uri.as_str() },
                "position": { "line": line, "character": ch }
            }),
        );
        assert!(
            resp.result.as_ref().is_none_or(|v| v.is_null()),
            "expected null definition on .s line {line}: {:?}",
            resp.result
        );
    }
    let def_raw_inc = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": raw_uri.as_str() },
            "position": { "line": 1, "character": 16 }
        }),
    );
    assert_eq!(
        def_raw_inc.result.as_ref().unwrap()["uri"],
        json!(inc_uri.as_str())
    );
    let def_raw_equ = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": raw_uri.as_str() },
            "position": { "line": 6, "character": 14 }
        }),
    );
    assert_eq!(
        def_raw_equ.result.as_ref().unwrap()["uri"],
        json!(inc_uri.as_str())
    );

    // `.s` with `-x assembler-with-cpp` resolves `#include` and `#define`
    let def_forced_a = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": forced_uri.as_str() },
            "position": { "line": 3, "character": 11 }
        }),
    );
    assert_eq!(
        def_forced_a.result.as_ref().unwrap()["range"]["start"]["line"],
        1
    );
    let def_forced_hdr = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": forced_uri.as_str() },
            "position": { "line": 4, "character": 14 }
        }),
    );
    assert_eq!(
        def_forced_hdr.result.as_ref().unwrap()["uri"],
        json!(defs_uri.as_str())
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn location_counter_dot_excluded_from_symbols_refs_and_semantic_tokens() {
    let mut client = LspClient::start(None, None);
    let uri = crate::common::test_uri("location_counter.S");
    let src = "\
.equ ALIGN_BYTES, 8
. = ALIGN_BYTES
. = . + 16
.Lkeep:
    nop
";
    client.open_doc(&uri, 1, src);

    // Document symbols contain ALIGN_BYTES and .Lkeep, never `.`
    let doc_syms = client.send_request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );
    let sym_names: Vec<String> = doc_syms
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        sym_names.contains(&"ALIGN_BYTES".to_owned()),
        "got: {sym_names:?}"
    );
    assert!(
        sym_names.contains(&".Lkeep".to_owned()),
        "got: {sym_names:?}"
    );
    assert!(
        !sym_names.contains(&".".to_owned()),
        "`.` leaked into doc symbols: {sym_names:?}"
    );

    // Definition / references on `.` return null / empty
    let def_dot = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 4 }
        }),
    );
    assert!(def_dot.result.is_none_or(|v| v.is_null()));

    // Semantic tokens must not emit a token at column 0 of line 1 or columns 0 / 4 of line 2
    let st = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );
    let data = st.result.expect("semantic tokens")["data"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut line = 0u64;
    let mut ch = 0u64;
    let mut token_positions = Vec::new();
    for chunk in data.as_chunks::<5>().0 {
        let dl = chunk[0].as_u64().unwrap();
        let ds = chunk[1].as_u64().unwrap();
        line += dl;
        ch = if dl == 0 { ch + ds } else { ds };
        token_positions.push((line, ch));
    }
    assert!(
        !token_positions.contains(&(1, 0)),
        "dot emitted at (1,0): {token_positions:?}"
    );
    assert!(
        !token_positions.contains(&(2, 0)),
        "dot emitted at (2,0): {token_positions:?}"
    );
    assert!(
        !token_positions.contains(&(2, 4)),
        "dot emitted at (2,4): {token_positions:?}"
    );
    assert!(
        token_positions.contains(&(1, 4)),
        "ALIGN_BYTES ref should remain: {token_positions:?}"
    );

    client.shutdown_and_exit();
}

#[test]
fn rename_allows_directive_stem_labels_rejects_instructions_and_ignores_comments() {
    let mut client = LspClient::start(None, None);
    let uri = crate::common::test_uri("rename_edge.S");
    let src = "\
data:
    nop
// see data in comment
foo:
    j data
";
    client.open_doc(&uri, 1, src);

    // `data` is a valid label (not `.data`), so prepareRename & rename succeed
    let prep_data = client.send_request(
        "textDocument/prepareRename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 1 }
        }),
    );
    assert!(
        prep_data.result.as_ref().is_some_and(|v| !v.is_null()),
        "expected prepareRename on `data:` to succeed"
    );

    let ren_data = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 1 },
            "newName": "rodata_tbl"
        }),
    );
    assert!(ren_data.error.is_none());
    assert_eq!(
        ren_data.result.as_ref().unwrap()["changes"][uri.as_str()]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Renaming to an instruction mnemonic (`addi`) must be rejected
    let ren_inst = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 3, "character": 1 },
            "newName": "addi"
        }),
    );
    assert!(ren_inst.error.is_some());

    // Navigation & rename inside `// see data in comment` must return null
    let def_comment = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 8 }
        }),
    );
    assert!(def_comment.result.as_ref().is_none_or(|v| v.is_null()));

    let prep_comment = client.send_request(
        "textDocument/prepareRename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 8 }
        }),
    );
    assert!(prep_comment.result.as_ref().is_none_or(|v| v.is_null()));

    client.shutdown_and_exit();
}

#[test]
fn new_lsp_providers_and_workspace_symbol_local_label_filter() {
    let uri = crate::common::test_uri("nav_features.S");
    let src = "\
.equ MY_CONST, 42
.macro PUSH r
    sd \\r, 0(sp)
.endm
main_fn:
    addi a0, a1, MY_CONST
    csrr t0, mtvec
.Linner_loop:
    j .Linner_loop
";
    let mut client = LspClient::start(
        Some(json!({ "defaultMarch": "rv64gc", "diagnostics": { "enable": false } })),
        None,
    );
    let caps = &client.init_result.capabilities;
    assert!(caps.signature_help_provider.is_some());
    assert!(caps.folding_range_provider.is_some());
    assert!(caps.inlay_hint_provider.is_some());

    client.open_doc(&uri, 1, src);

    let ws_resp = client.send_request("workspace/symbol", json!({ "query": "" }));
    let ws_names: Vec<String> = ws_resp
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(ws_names.iter().any(|n| n == "main_fn"), "{ws_names:?}");
    assert!(
        !ws_names.iter().any(|n| n == ".Linner_loop"),
        "local label leaked into workspace symbols: {ws_names:?}"
    );

    let sig = client.send_request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": uri.as_str() }, "position": { "line": 5, "character": 13 } }),
    );
    let sig_val = sig.result.expect("signatureHelp result");
    assert_eq!(sig_val["activeParameter"], json!(1), "{sig_val}");

    let folds = client.send_request(
        "textDocument/foldingRange",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );
    let fold_arr = folds
        .result
        .expect("foldingRange")
        .as_array()
        .unwrap()
        .clone();
    assert!(
        fold_arr
            .iter()
            .any(|f| f["startLine"] == 1 && f["endLine"] == 3),
        "expected .macro fold in {fold_arr:?}"
    );

    let hints = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 8, "character": 0 }
            }
        }),
    );
    let hint_labels: Vec<String> = hints
        .result
        .expect("inlayHint")
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["label"].as_str().map(str::to_owned))
        .collect();
    assert!(
        hint_labels.iter().any(|l| l.contains("0x305")),
        "missing mtvec CSR hint in {hint_labels:?}"
    );
    assert!(
        hint_labels.iter().any(|l| l.contains("42")),
        "missing MY_CONST value hint in {hint_labels:?}"
    );

    client.shutdown_and_exit();
}

#[test]
fn d4_fallbacks_and_d5_inline_macro_labels_support_navigation_and_symbols() {
    let mut client = LspClient::start(None, None);
    let uri = crate::common::test_uri("d4_d5_nav.S");
    let src = "\
SYM_BASE = 8
SYM_DERIVED = SYM_BASE + 4
.text
_start:
    SET_FAULT_HANDLER(x0, target_handler)
    .dc.a target_handler
    ALTERNATIVE(\"nop\", \"1: nop\\n\\tj 1b\", 0)
    ALTERNATIVE(nop, __stringify(inline_alt: addi a0, a0, 1; j inline_alt), 0)
target_handler:
    li a0, SYM_DERIVED
";
    client.open_doc(&uri, 1, src);

    // Column-0 `sym = expr`: go-to-definition from SYM_BASE on line 1 to line 0, and SYM_DERIVED on line 9 to line 1.
    let def_base = client.send_request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri.as_str() }, "position": { "line": 1, "character": 15 } }),
    );
    assert_eq!(
        def_base.result.as_ref().unwrap()["range"]["start"]["line"],
        json!(0),
        "SYM_BASE def: {def_base:?}"
    );

    // `.dc.a target_handler` reference jumps to `target_handler:` on line 8.
    let def_dca = client.send_request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri.as_str() }, "position": { "line": 5, "character": 12 } }),
    );
    assert_eq!(
        def_dca.result.as_ref().unwrap()["range"]["start"]["line"],
        json!(8),
        ".dc.a target_handler def: {def_dca:?}"
    );

    // D5 `__stringify(inline_alt: ... j inline_alt)` resolves definition + references + documentSymbol.
    let def_alt = client.send_request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri.as_str() }, "position": { "line": 7, "character": 66 } }),
    );
    assert_eq!(
        def_alt.result.as_ref().unwrap()["range"]["start"]["line"],
        json!(7),
        "inline_alt def: {def_alt:?}"
    );

    let sym_resp = client.send_request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );
    let sym_json = serde_json::to_string(&sym_resp.result).unwrap();
    assert!(sym_json.contains("SYM_BASE"), "{sym_json}");
    assert!(sym_json.contains("inline_alt"), "{sym_json}");

    client.shutdown_and_exit();
}
