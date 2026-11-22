use crate::common::{LspClient, fixtures_dir, path_to_uri};
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
