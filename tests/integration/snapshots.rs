use serde_json::{Value, json};

use crate::common::snapshot::{assert_json_snapshot, assert_text_snapshot, normalize_text};
use crate::common::{
    LspClient, completion_items_vec, fixtures_dir, has_tool, hover_markdown, path_to_uri,
};

#[test]
fn snapshot_hovers() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let fix_path_str = fixtures.to_string_lossy().into_owned();
    let fix_uri_str = root_uri.as_str().to_owned();
    let norm = |s: &str| {
        normalize_text(
            s,
            &[
                (fix_uri_str.as_str(), "<FIXTURES_URI>"),
                (fix_path_str.as_str(), "<FIXTURES>"),
            ],
        )
    };

    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang",
            "llvmMc": "/nonexistent/llvm-mc",
            "diagnostics": { "enable": false }
        })),
        Some(&root_uri),
    );
    client.open_doc(&kernel_uri, 1, &kernel_src);
    // Drain background workspace scan deterministically before semantic tokens
    let _ = client.send_request("workspace/symbol", json!({ "query": "" }));

    let cases = [
        (
            "hover/preprocessor_directive.md",
            0,
            2,
            "preprocessor_directive",
        ),
        ("hover/equ.md", 2, 6, "equ"),
        ("hover/macro.md", 5, 8, "macro"),
        ("hover/macro_param.md", 6, 8, "macro_param"),
        ("hover/directive.md", 10, 6, "directive"),
        ("hover/register.md", 12, 10, "register"),
        ("hover/pseudo.md", 14, 5, "pseudo"),
        ("hover/define.md", 15, 12, "define"),
        ("hover/csr.md", 16, 10, "csr"),
        ("hover/alias.md", 17, 5, "alias"),
        ("hover/label.md", 26, 2, "label"),
        ("hover/instruction.md", 28, 5, "instruction"),
    ];

    for (snap, line, col, label) in cases {
        let md = hover_markdown(&mut client, &kernel_uri, line, col)
            .unwrap_or_else(|| panic!("expected hover for {label} at {line}:{col}"));
        assert_text_snapshot(snap, &norm(&md));
    }

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires clang and llvm-mc"]
fn snapshot_hover_with_expansion() {
    if !has_tool("llvm-mc") || !has_tool("clang") {
        eprintln!("skipping: clang or llvm-mc unavailable");
        return;
    }
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&kernel_uri, 1, &kernel_src);
    // Drain background workspace scan deterministically before semantic tokens
    let _ = client.send_request("workspace/symbol", json!({ "query": "" }));

    // `addi sp, sp, -16` encodes with exact 32-bit hex
    let md_addi = hover_markdown(&mut client, &kernel_uri, 28, 5).expect("addi expansion hover");
    assert_text_snapshot("hover/instruction_with_expansion.md", &md_addi);

    // `la sp, _stack_top` expands across multiple instructions with relocation
    let md_la = hover_markdown(&mut client, &kernel_uri, 14, 5).expect("la expansion hover");
    assert_text_snapshot("hover/macro_expansion.md", &md_la);

    client.shutdown_and_exit();
}

fn canonical_completion(
    client: &mut LspClient,
    uri: &lsp_types::Uri,
    line: u32,
    character: u32,
    max_resolved_docs: usize,
) -> Value {
    let resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": line, "character": character }
        }),
    );
    assert!(resp.error.is_none(), "completion error: {:?}", resp.error);
    let items = completion_items_vec(&resp.result);
    let is_incomplete = resp
        .result
        .as_ref()
        .and_then(|v| v.get("isIncomplete"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);

    let out_items: Vec<Value> = items
        .into_iter()
        .enumerate()
        .map(|(idx, mut item)| {
            if item.get("documentation").is_none_or(Value::is_null)
                && !item.get("data").is_none_or(Value::is_null)
                && idx < max_resolved_docs
            {
                let res = client.send_request("completionItem/resolve", item.clone());
                if let Some(resolved) = res.result {
                    item = resolved;
                }
            }
            json!({
                "label": item.get("label").cloned().unwrap_or(Value::Null),
                "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                "detail": item.get("detail").cloned().unwrap_or(Value::Null),
                "insertText": item.get("textEdit")
                    .and_then(|te| te.get("newText"))
                    .or_else(|| item.get("insertText"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "documentation": item.get("documentation")
                    .and_then(|d| d.get("value").or(Some(d)))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect();

    json!({
        "isIncomplete": is_incomplete,
        "items": out_items,
    })
}

#[test]
fn snapshot_completions() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_snap_comp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let root_uri = path_to_uri(&tmp);
    let mut client = LspClient::start_with_snippets(
        Some(json!({
            "defaultMarch": "rv64gcv",
            "diagnostics": { "enable": false }
        })),
        Some(&root_uri),
    );
    let file_path = tmp.join("snap_completion_virtual.S");
    std::fs::write(&file_path, "").expect("write empty placeholder");
    let uri = path_to_uri(&file_path);
    let src = "\
target_alpha:
target_beta:
    add
    addi 
    fadd.s 
    vadd.vv 
    csrr a0, st
    fence 
    fcvt.w.s a0, fa0, 
    .option 
    lui t0, %
    j 
";
    client.open_doc(&uri, 1, src);

    let cases = [
        ("completion/statement_start.json", 2, 7, 10),
        ("completion/operand_int_reg.json", 3, 9, 5),
        ("completion/operand_float_reg.json", 4, 11, 5),
        ("completion/operand_vec_reg.json", 5, 12, 5),
        ("completion/operand_csr.json", 6, 15, 5),
        ("completion/operand_fence.json", 7, 10, 10),
        ("completion/operand_frm.json", 8, 22, 10),
        ("completion/operand_option.json", 9, 12, 10),
        ("completion/operand_reloc.json", 10, 13, 15),
        ("completion/operand_symbol.json", 11, 6, 10),
    ];

    for (snap, line, col, docs) in cases {
        let val = canonical_completion(&mut client, &uri, line, col, docs);
        assert_json_snapshot(snap, &val);
    }

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn snapshot_semantic_tokens_and_symbols_and_rename() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&kernel_uri, 1, &kernel_src);
    // Drain background workspace scan deterministically before semantic tokens
    let _ = client.send_request("workspace/symbol", json!({ "query": "" }));

    // Decode semantic tokens using legend from initializeResult
    let legend = client
        .init_result
        .capabilities
        .semantic_tokens_provider
        .as_ref()
        .expect("semantic tokens capability");
    let legend_json = serde_json::to_value(legend).expect("legend json");
    let token_types: Vec<String> = legend_json["legend"]["tokenTypes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let token_mods: Vec<String> = legend_json["legend"]["tokenModifiers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();

    let st_resp = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": kernel_uri.as_str() } }),
    );
    let data = st_resp.result.expect("semantic tokens")["data"]
        .as_array()
        .unwrap()
        .clone();
    let mut line = 0u64;
    let mut ch = 0u64;
    let mut decoded = Vec::new();
    for chunk in data.as_chunks::<5>().0 {
        let dl = chunk[0].as_u64().unwrap();
        let ds = chunk[1].as_u64().unwrap();
        let len = chunk[2].as_u64().unwrap();
        let ty = chunk[3].as_u64().unwrap() as usize;
        let mods = chunk[4].as_u64().unwrap();
        line += dl;
        ch = if dl == 0 { ch + ds } else { ds };
        let mod_names: Vec<&str> = token_mods
            .iter()
            .enumerate()
            .filter(|(i, _)| (mods & (1 << i)) != 0)
            .map(|(_, s)| s.as_str())
            .collect();
        decoded.push(json!({
            "line": line,
            "char": ch,
            "length": len,
            "type": token_types.get(ty).map(String::as_str).unwrap_or("?"),
            "modifiers": mod_names,
        }));
    }
    assert_json_snapshot("semantic_tokens/kernel_entry.json", &Value::Array(decoded));

    // Document symbols
    let sym_resp = client.send_request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": kernel_uri.as_str() } }),
    );
    assert_json_snapshot(
        "symbols/kernel_entry.json",
        sym_resp.result.as_ref().expect("document symbols"),
    );

    // Rename `.Llocal_helper` (line 34 col 2) and `SAVE_REG` (line 5 col 8)
    let fix_uri_str = root_uri.as_str().to_owned();
    let ren_local = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 34, "character": 2 },
            "newName": ".Llocal_renamed"
        }),
    );
    let local_raw = serde_json::to_string_pretty(&ren_local.result).unwrap();
    let local_norm: Value = serde_json::from_str(&normalize_text(
        &local_raw,
        &[(&fix_uri_str, "<FIXTURES_URI>")],
    ))
    .unwrap();
    assert_json_snapshot("rename/local_helper.json", &local_norm);

    let ren_macro = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 5, "character": 8 },
            "newName": "PUSH_REG"
        }),
    );
    let macro_raw = serde_json::to_string_pretty(&ren_macro.result).unwrap();
    let macro_norm: Value = serde_json::from_str(&normalize_text(
        &macro_raw,
        &[(&fix_uri_str, "<FIXTURES_URI>")],
    ))
    .unwrap();
    assert_json_snapshot("rename/save_reg.json", &macro_norm);

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires clang"]
fn snapshot_clang_diagnostics() {
    if !has_tool("clang") {
        eprintln!("skipping: clang unavailable");
        return;
    }
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let err_path = fixtures.join("errors.S");
    let err_uri = path_to_uri(&err_path);
    let err_src = std::fs::read_to_string(&err_path).expect("read errors.S");

    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 10, "external": "on" } })),
        Some(&root_uri),
    );
    client.open_doc(&err_uri, 1, &err_src);
    let diag_notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("publishDiagnostics notification");
    let diags = &diag_notif.params["diagnostics"];
    let fix_uri_str = root_uri.as_str().to_owned();
    let raw = serde_json::to_string_pretty(diags).unwrap();
    let norm: Value =
        serde_json::from_str(&normalize_text(&raw, &[(&fix_uri_str, "<FIXTURES_URI>")])).unwrap();
    assert_json_snapshot("diagnostics/clang_errors.json", &norm);

    client.shutdown_and_exit();
}
