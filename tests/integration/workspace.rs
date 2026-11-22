use crate::common::{
    LspClient, completion_labels, fixtures_dir, path_to_uri, workspace_symbol_names,
};
use lsp_types::{Position, Range, TextDocumentSyncCapability, TextDocumentSyncKind};
use serde_json::json;

#[test]
fn initialize_capabilities_and_shutdown() {
    let client = LspClient::start(None, None);
    assert_eq!(
        client.init_result.capabilities.text_document_sync,
        Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL
        ))
    );
    assert!(client.init_result.capabilities.hover_provider.is_some());
    assert!(
        client
            .init_result
            .capabilities
            .completion_provider
            .as_ref()
            .and_then(|c| c.resolve_provider)
            == Some(true),
        "expected resolve_provider capability"
    );
    assert!(
        client
            .init_result
            .capabilities
            .semantic_tokens_provider
            .is_some(),
        "expected semantic_tokens_provider capability"
    );
    assert!(
        client
            .init_result
            .capabilities
            .definition_provider
            .is_some()
    );
    assert!(
        client
            .init_result
            .capabilities
            .references_provider
            .is_some()
    );
    assert!(
        client
            .init_result
            .capabilities
            .code_action_provider
            .is_some()
    );
    client.shutdown_and_exit();
}

#[test]
fn unknown_method_returns_method_not_found_without_crashing() {
    let mut client = LspClient::start(None, None);
    let resp = client.send_request("riscv/nonexistentMethod", json!({}));
    let err = resp.error.expect("expected JSON-RPC error");
    assert_eq!(err["code"], json!(-32601));
    client.shutdown_and_exit();
}

#[test]
fn regression_multibyte_and_out_of_range_edits_never_panic() {
    let mut client = LspClient::start(None, None);
    let path = fixtures_dir().join("multibyte.S");
    let text = std::fs::read_to_string(&path).expect("read multibyte.S");
    let uri = path_to_uri(&path);

    client.open_doc(&uri, 1, &text);
    // Edit right in the middle of a surrogate pair emoji on line 1, empty line, and out-of-range
    client.change_doc_incremental(
        &uri,
        2,
        Range::new(Position::new(1, 15), Position::new(1, 16)),
        "X",
    );
    client.change_doc_incremental(
        &uri,
        3,
        Range::new(Position::new(7, 0), Position::new(7, 0)),
        "    nop",
    );
    client.change_doc_incremental(
        &uri,
        4,
        Range::new(Position::new(999, 999), Position::new(1000, 1000)),
        "\n# trailing\n",
    );

    client.shutdown_and_exit();
}

#[test]
fn compile_commands_json_overrides_default_march() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_cc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let src_file = tmp.join("boot.S");
    std::fs::write(&src_file, "    \n").expect("write boot.S");
    let cc_json = json!([{
        "directory": tmp.to_string_lossy(),
        "command": format!("clang --target=riscv32-unknown-elf -march=rv32i -c {}", src_file.display()),
        "file": src_file.to_string_lossy()
    }]);
    std::fs::write(
        tmp.join("compile_commands.json"),
        serde_json::to_string_pretty(&cc_json).expect("json"),
    )
    .expect("write compile_commands.json");

    let root_uri = path_to_uri(&tmp);
    let doc_uri = path_to_uri(&src_file);
    let mut client = LspClient::start(Some(json!({ "defaultMarch": "rv64gc" })), Some(&root_uri));
    client.open_doc(&doc_uri, 1, "    \n");

    let comp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": doc_uri.as_str() },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let labels = completion_labels(&comp.result);
    assert!(labels.iter().any(|l| l == "addi"));
    assert!(!labels.iter().any(|l| l == "mul"));

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn watched_files_dynamic_registration_updates_unopened_file_symbols() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_watch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let file_a = tmp.join("a.S");
    let file_b = tmp.join("b.S");
    std::fs::write(&file_a, "initial_watched_sym:\n    nop\n").expect("write a.S");

    let root_uri = path_to_uri(&tmp);
    let uri_a = path_to_uri(&file_a);
    let mut client = LspClient::start_with_watched_files(None, Some(&root_uri));

    let reg = client
        .wait_for_notification(
            "client/registerCapability",
            std::time::Duration::from_secs(3),
        )
        .expect("expected client/registerCapability for workspace/didChangeWatchedFiles");
    let registrations = reg.params["registrations"]
        .as_array()
        .expect("registrations array");
    assert!(
        registrations
            .iter()
            .any(|r| r["method"] == "workspace/didChangeWatchedFiles"),
        "missing didChangeWatchedFiles registration: {registrations:?}"
    );

    let initial_syms = workspace_symbol_names(&mut client, "initial_watched_sym");
    assert!(
        initial_syms.iter().any(|s| s == "initial_watched_sym"),
        "initial symbol missing: {initial_syms:?}"
    );

    std::fs::write(&file_a, "updated_watched_sym:\n    nop\n").expect("update a.S");
    std::fs::write(&file_b, "created_watched_sym:\n    nop\n").expect("write b.S");
    let uri_b = path_to_uri(&file_b);

    client.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [
                { "uri": uri_a.as_str(), "type": 2 },
                { "uri": uri_b.as_str(), "type": 1 }
            ]
        }),
    );

    let after_update = workspace_symbol_names(&mut client, "watched_sym");
    assert!(after_update.iter().any(|s| s == "updated_watched_sym"));
    assert!(after_update.iter().any(|s| s == "created_watched_sym"));
    assert!(!after_update.iter().any(|s| s == "initial_watched_sym"));

    std::fs::remove_file(&file_b).expect("remove b.S");
    client.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [
                { "uri": uri_b.as_str(), "type": 3 }
            ]
        }),
    );
    let after_delete = workspace_symbol_names(&mut client, "created_watched_sym");
    assert!(after_delete.is_empty());

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn background_index_skips_hidden_and_gitignored_and_reports_progress() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_bg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".hidden")).unwrap();
    std::fs::create_dir_all(tmp.join("ignored_dir")).unwrap();
    std::fs::write(tmp.join(".gitignore"), "ignored_dir/\n").unwrap();
    std::fs::write(
        tmp.join(".hidden/secret.S"),
        "hidden_secret_sym:\n    nop\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("ignored_dir/ignored.S"),
        "gitignored_sym:\n    nop\n",
    )
    .unwrap();
    std::fs::write(tmp.join("only_header.h"), "#define UNREACHABLE_HDR 1\n").unwrap();
    std::fs::write(tmp.join("visible.S"), "visible_ws_sym:\n    nop\n").unwrap();

    let root_uri = path_to_uri(&tmp);
    let mut client = LspClient::start_with_work_done_progress(None, Some(&root_uri));

    let create = client
        .wait_for_notification(
            "window/workDoneProgress/create",
            std::time::Duration::from_secs(3),
        )
        .expect("window/workDoneProgress/create");
    assert_eq!(create.params["token"], json!("riscv-asm-lsp/index"));

    let mut saw_begin = false;
    let mut saw_end = false;
    for _ in 0..4 {
        let Some(p) = client.wait_for_notification("$/progress", std::time::Duration::from_secs(3))
        else {
            break;
        };
        if p.params["value"]["kind"] == "begin" {
            saw_begin = true;
        }
        if p.params["value"]["kind"] == "end" {
            saw_end = true;
            break;
        }
    }
    assert!(saw_begin, "expected $/progress begin");
    assert!(saw_end, "expected $/progress end");

    let syms = workspace_symbol_names(&mut client, "");
    assert!(syms.iter().any(|s| s == "visible_ws_sym"), "syms: {syms:?}");
    assert!(!syms.iter().any(|s| s == "hidden_secret_sym"));
    assert!(!syms.iter().any(|s| s == "gitignored_sym"));
    assert!(!syms.iter().any(|s| s == "UNREACHABLE_HDR"));

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn priority_scan_resolves_project_before_go_assembly_and_skips_it() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_prio_ws_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let go_mod = tmp.join("go/pkg/mod/some_pkg");
    let proj = tmp.join("projects/my_rv");
    std::fs::create_dir_all(&go_mod).unwrap();
    std::fs::create_dir_all(proj.join(".git")).unwrap();

    for i in 0..100 {
        std::fs::write(
            go_mod.join(format!("go_{i:03}.s")),
            format!(
                "#include \"textflag.h\"\nTEXT \u{00b7}go_sym_{i:03}(SB), NOSPLIT, $0-0\n    RET\n"
            ),
        )
        .unwrap();
    }
    let main_s = proj.join("main.S");
    let lib_s = proj.join("lib.S");
    std::fs::write(&main_s, "_start:\n    call rv_helper\n").unwrap();
    std::fs::write(&lib_s, "rv_helper:\n    ret\n").unwrap();

    let root_uri = path_to_uri(&tmp);
    let main_uri = path_to_uri(&main_s);
    let lib_uri = path_to_uri(&lib_s);
    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&main_uri, 1, "_start:\n    call rv_helper\n");

    let start_t = std::time::Instant::now();
    let def = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": main_uri.as_str() },
            "position": { "line": 1, "character": 11 }
        }),
    );
    assert!(
        start_t.elapsed() < std::time::Duration::from_secs(2),
        "priority lookup took {:?}",
        start_t.elapsed()
    );
    assert_eq!(
        def.result.as_ref().and_then(|r| r["uri"].as_str()),
        Some(lib_uri.as_str())
    );

    let syms = workspace_symbol_names(&mut client, "go_sym");
    assert!(syms.is_empty(), "Go Plan 9 symbols leaked: {syms:?}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn large_file_rapid_incremental_edits_and_hover() {
    use std::fmt::Write;
    let mut src = String::with_capacity(120_000);
    for i in 0..2_000 {
        let _ = writeln!(src, "lbl_{i:04}:\n    addi a0, a0, 1");
    }
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("large_edit_virtual.S"));
    client.open_doc(&uri, 1, &src);

    for step in 0..20 {
        client.change_doc_incremental(
            &uri,
            step + 2,
            Range::new(Position::new(1, 17), Position::new(1, 18)),
            &((step % 9) + 1).to_string(),
        );
    }

    let hover = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 6 }
        }),
    );
    assert!(hover.error.is_none());
    let md = hover.result.as_ref().unwrap()["contents"]["value"]
        .as_str()
        .unwrap_or("");
    assert!(md.contains("Add immediate"), "got: {md}");

    client.shutdown_and_exit();
}

#[test]
fn watched_files_registration_shutdown_exits_cleanly() {
    // 1. Answer registration before shutdown
    let mut client = LspClient::start_with_watched_files(None, None);
    let reg = client
        .wait_for_notification(
            "client/registerCapability",
            std::time::Duration::from_secs(3),
        )
        .expect("expected client/registerCapability");
    let reg_id = reg.id.expect("registerCapability request id");
    client.send_response(reg_id, json!(null));
    let (status, stderr) = client.shutdown_and_exit_with_stderr();
    assert!(
        status.success(),
        "expected exit 0 after watched files registration, got {status}; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("fatal error"),
        "unexpected fatal error in stderr:\n{stderr}"
    );

    // 2. Race case: client sends registration response after `shutdown` and before `exit`
    let mut client_race = LspClient::start_with_watched_files(None, None);
    let reg2 = client_race
        .wait_for_notification(
            "client/registerCapability",
            std::time::Duration::from_secs(3),
        )
        .expect("expected client/registerCapability");
    let reg2_id = reg2.id.expect("registerCapability request id");
    let shut_resp = client_race.send_request("shutdown", json!(null));
    assert!(shut_resp.error.is_none());
    client_race.send_response(reg2_id, json!(null));
    let status2 = client_race.exit_and_wait();
    assert!(
        status2.success(),
        "expected exit 0 when response arrives between shutdown and exit, got {status2}"
    );
}

#[test]
fn exit_without_shutdown_exits_with_code_1() {
    let client = LspClient::start(None, None);
    let status = client.exit_and_wait();
    assert_eq!(status.code(), Some(1), "expected exit code 1, got {status}");
}

#[test]
fn closing_stdin_exits_cleanly() {
    let client = LspClient::start(None, None);
    let status = client.close_stdin_and_wait();
    assert!(status.success(), "expected exit 0 on EOF, got {status}");
}

#[test]
fn linker_script_templates_are_skipped_in_scan_and_features() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_lds_skip_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let lds_path = tmp.join("vmlinux.lds.S");
    let lds_src = "\
SECTIONS {
    . = ALIGN(8);
    lds_leaked_sym:
    addi a0, zero, 1
}
";
    std::fs::write(&lds_path, lds_src).expect("write vmlinux.lds.S");
    let asm_path = tmp.join("entry.S");
    std::fs::write(&asm_path, "asm_visible_sym:\n    nop\n").expect("write entry.S");

    let root_uri = path_to_uri(&tmp);
    let lds_uri = path_to_uri(&lds_path);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20 } })),
        Some(&root_uri),
    );

    // Workspace scan must skip `vmlinux.lds.S` while still indexing `entry.S`
    let syms = workspace_symbol_names(&mut client, "");
    assert!(
        syms.iter().any(|s| s == "asm_visible_sym"),
        "syms: {syms:?}"
    );
    assert!(
        !syms.iter().any(|s| s == "lds_leaked_sym"),
        "linker script symbol leaked: {syms:?}"
    );

    // Opening a linker script template produces no symbols, completion, hover, semantic tokens, or diagnostics
    client.open_doc(&lds_uri, 1, lds_src);
    assert!(
        client
            .send_request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": lds_uri.as_str() } })
            )
            .result
            .is_none_or(|v| v.is_null())
    );
    assert!(
        client
            .send_request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": lds_uri.as_str() },
                    "position": { "line": 3, "character": 5 }
                })
            )
            .result
            .is_none_or(|v| v.is_null())
    );
    assert!(
        client
            .send_request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": lds_uri.as_str() },
                    "position": { "line": 3, "character": 8 }
                })
            )
            .result
            .is_none_or(|v| v.is_null())
    );
    assert!(
        client
            .send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": lds_uri.as_str() } })
            )
            .result
            .is_none_or(|v| v.is_null())
    );

    let syms_after = workspace_symbol_names(&mut client, "lds_leaked");
    assert!(syms_after.is_empty(), "leaked after open: {syms_after:?}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}
