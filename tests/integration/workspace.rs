use crate::common::{
    LspClient, completion_labels, fixtures_dir, hover_markdown, path_to_uri, workspace_symbol_names,
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
    assert!(
        client
            .init_result
            .capabilities
            .call_hierarchy_provider
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

#[test]
fn non_riscv_target_stays_silent_in_lsp_and_index() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_non_rv_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let x86_path = tmp.join("boot_x86.S");
    let x86_src = "x86_hidden_sym:\n    add %eax, %ebx\n    call x86_hidden_sym\n";
    std::fs::write(&x86_path, x86_src).unwrap();

    let rv_path = tmp.join("boot_rv.S");
    let rv_src = "rv_visible_sym:\n    nop\n";
    std::fs::write(&rv_path, rv_src).unwrap();

    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "--target=x86_64-linux-gnu", "-c", x86_path.to_string_lossy()],
                "file": x86_path.to_string_lossy()
            },
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-c", rv_path.to_string_lossy()],
                "file": rv_path.to_string_lossy()
            }
        ])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let x86_uri = path_to_uri(&x86_path);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20 } })),
        Some(&root_uri),
    );

    let syms = workspace_symbol_names(&mut client, "");
    assert!(syms.iter().any(|s| s == "rv_visible_sym"), "syms: {syms:?}");
    assert!(
        !syms.iter().any(|s| s == "x86_hidden_sym"),
        "leaked: {syms:?}"
    );

    client.open_doc(&x86_uri, 1, x86_src);

    for (method, params) in [
        (
            "textDocument/hover",
            json!({ "textDocument": { "uri": x86_uri.as_str() }, "position": { "line": 1, "character": 5 } }),
        ),
        (
            "textDocument/completion",
            json!({ "textDocument": { "uri": x86_uri.as_str() }, "position": { "line": 1, "character": 7 } }),
        ),
        (
            "textDocument/definition",
            json!({ "textDocument": { "uri": x86_uri.as_str() }, "position": { "line": 2, "character": 12 } }),
        ),
        (
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": x86_uri.as_str() } }),
        ),
        (
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": x86_uri.as_str() } }),
        ),
    ] {
        let resp = client.send_request(method, params);
        assert!(
            resp.result.as_ref().is_none_or(|v| v.is_null()),
            "expected null for {method} on x86_64 target, got {:?}",
            resp.result
        );
    }

    assert!(
        client
            .wait_for_notification(
                "textDocument/publishDiagnostics",
                std::time::Duration::from_millis(120),
            )
            .is_none(),
        "unexpected diagnostics published for non-RISC-V target"
    );

    let syms_after = workspace_symbol_names(&mut client, "x86_hidden");
    assert!(syms_after.is_empty(), "leaked after open: {syms_after:?}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn workspace_did_change_configuration_compile_flags_and_multi_root() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_cfg_{}", std::process::id()));
    let root_a = tmp.join("root_a");
    let root_b = tmp.join("root_b");
    let inc_a = root_a.join("inc_a");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&inc_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    std::fs::write(
        root_a.join("compile_flags.txt"),
        "-march=rv32imac\n-Iinc_a\n",
    )
    .unwrap();
    std::fs::write(inc_a.join("hdr.h"), "#define FLAG_VAL 77\n").unwrap();
    let a_path = root_a.join("a.S");
    let a_src = "#include \"hdr.h\"\n    fadd.s ft0, ft1, ft2\n    li a0, FLAG_VAL\n";
    std::fs::write(&a_path, a_src).unwrap();

    let b_path = root_b.join("b.S");
    let b_src = "b_sym:\n    fadd.s ft0, ft1, ft2\n";
    std::fs::write(&b_path, b_src).unwrap();

    let root_a_uri = path_to_uri(&root_a);
    let root_b_uri = path_to_uri(&root_b);
    let a_uri = path_to_uri(&a_path);
    let b_uri = path_to_uri(&b_path);

    let mut client = LspClient::start(
        Some(json!({ "defaultMarch": "rv64gc", "diagnostics": { "enable": false } })),
        Some(&root_a_uri),
    );
    client.open_doc(&a_uri, 1, a_src);

    let h_fadd_a = hover_markdown(&mut client, &a_uri, 1, 6).expect("fadd.s hover in a.S");
    assert!(
        h_fadd_a.to_ascii_lowercase().contains("not enabled"),
        "expected compile_flags.txt rv32imac to warn on fadd.s, got: {h_fadd_a}"
    );
    let h_flag_a = hover_markdown(&mut client, &a_uri, 2, 12).expect("FLAG_VAL hover");
    assert!(
        h_flag_a.contains("77"),
        "expected -Iinc_a from compile_flags.txt to resolve hdr.h, got: {h_flag_a}"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [{ "uri": root_b_uri.as_str(), "name": "root_b" }],
                "removed": []
            }
        }),
    );

    let syms = workspace_symbol_names(&mut client, "b_sym");
    assert!(
        syms.iter().any(|s| s == "b_sym"),
        "expected b_sym after adding root_b, got: {syms:?}"
    );

    client.open_doc(&b_uri, 1, b_src);
    let h_fadd_b_before =
        hover_markdown(&mut client, &b_uri, 1, 6).expect("fadd.s hover before config");
    assert!(
        !h_fadd_b_before.to_ascii_lowercase().contains("not enabled"),
        "expected default rv64gc before config change: {h_fadd_b_before}"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "defaultMarch": "rv64imac", "diagnostics": { "enable": false } } }),
    );

    let h_fadd_b_after =
        hover_markdown(&mut client, &b_uri, 1, 6).expect("fadd.s hover after config");
    assert!(
        h_fadd_b_after.to_ascii_lowercase().contains("not enabled"),
        "expected didChangeConfiguration rv64imac to disable fadd.s: {h_fadd_b_after}"
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cross_directory_include_resolution_via_compile_commands() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_xdir_inc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let src_dir = tmp.join("arch").join("riscv");
    let inc_dir = tmp.join("include").join("generated");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&inc_dir).unwrap();

    let hdr = inc_dir.join("defs.h");
    std::fs::write(&hdr, "#define XDIR_MAGIC 123\n").unwrap();
    let main_s = src_dir.join("entry.S");
    let main_src = "#include \"generated/defs.h\"\n_start:\n    li a0, XDIR_MAGIC\n";
    std::fs::write(&main_s, main_src).unwrap();

    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy(),
            "arguments": [
                "clang",
                "--target=riscv64-unknown-elf",
                "-march=rv64gc",
                "-Iinclude",
                "-c",
                "arch/riscv/entry.S"
            ],
            "file": "arch/riscv/entry.S"
        }])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let main_uri = path_to_uri(&main_s);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&main_uri, 1, main_src);

    let hv = hover_markdown(&mut client, &main_uri, 2, 14).expect("XDIR_MAGIC hover");
    assert!(hv.contains("123"), "got: {hv}");

    let def = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": main_uri.as_str() },
            "position": { "line": 0, "character": 14 }
        }),
    );
    let def_str = serde_json::to_string(&def.result.expect("def result")).unwrap();
    assert!(def_str.contains("defs.h"), "got: {def_str}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(windows)]
#[test]
fn windows_drive_mixed_case_compile_db_and_encoded_colon_uri() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_win_case_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sub_upper = tmp.join("SubDir");
    std::fs::create_dir_all(&sub_upper).unwrap();
    let file_upper = sub_upper.join("MainFile.S");
    let src = "_start:\n    fadd.s ft0, ft1, ft2\n";
    std::fs::write(&file_upper, src).unwrap();

    // Write compile_commands.json with inverted path case
    let lower_file = file_upper.to_string_lossy().to_lowercase();
    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy().to_lowercase(),
            "arguments": ["clang", "--target=riscv32-unknown-elf", "-march=rv32imac", "-c", lower_file],
            "file": file_upper.to_string_lossy().to_lowercase()
        }])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let uri_normal = path_to_uri(&file_upper);
    let raw = uri_normal.as_str();
    let encoded_str = if let Some(rest) = raw.strip_prefix("file:///") {
        let mut chars = rest.chars();
        if let (Some(drive), Some(':')) = (chars.next(), chars.next()) {
            format!(
                "file:///{}%3A{}",
                drive.to_ascii_lowercase(),
                chars.as_str()
            )
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };
    let uri_encoded: lsp_types::Uri = encoded_str.parse().expect("encoded uri");

    let mut client = LspClient::start(
        Some(json!({ "defaultMarch": "rv64gc", "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&uri_normal, 1, src);

    // Querying hover via `%3A` lowercase-drive URI hits the same open document & mixed-case compile DB entry (`rv32imac` warns on `fadd.s`)
    let hv = hover_markdown(&mut client, &uri_encoded, 1, 6).expect("hover via encoded colon URI");
    assert!(
        hv.to_ascii_lowercase().contains("not enabled"),
        "expected mixed-case compile DB rv32imac to apply via encoded URI: {hv}"
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn compile_db_interpolation_and_project_yaml_live_reload() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_c1_c2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mod_dir = tmp.join("kernel");
    let local_inc = mod_dir.join("inc");
    std::fs::create_dir_all(&local_inc).unwrap();

    std::fs::write(local_inc.join("cfg.h"), "#define KERNEL_VAL 88\n").unwrap();
    let listed = mod_dir.join("listed.S");
    let unlisted = mod_dir.join("unlisted.S");
    std::fs::write(&listed, "_listed:\n    nop\n").unwrap();
    let unlisted_src = "#include \"cfg.h\"\n    .eqv UNLISTED_CONST, 1\n    fadd.s ft0, ft1, ft2\n    li a0, KERNEL_VAL\n";
    std::fs::write(&unlisted, unlisted_src).unwrap();

    // Only `listed.S` is in compile_commands.json (rv32imac); `unlisted.S` interpolates from it.
    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy(),
            "arguments": [
                "clang",
                "--target=riscv32-unknown-elf",
                "-march=rv32imac",
                "-c",
                listed.to_string_lossy()
            ],
            "file": listed.to_string_lossy()
        }])
        .to_string(),
    )
    .unwrap();

    // Project config in `kernel/.riscv-asm-lsp.yaml` adds `-Iinc` and suppresses `extension-disabled`.
    let yaml_path = mod_dir.join(".riscv-asm-lsp.yaml");
    std::fs::write(
        &yaml_path,
        "\
CompileFlags:
  Add:
    - \"-Iinc\"
Diagnostics:
  Suppress:
    - \"gnu-only-directive\"
",
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let unlisted_uri = path_to_uri(&unlisted);
    let yaml_uri = path_to_uri(&yaml_path);
    let mut client = LspClient::start(
        Some(json!({ "defaultMarch": "rv64gc", "diagnostics": { "enable": true, "delayMs": 20 } })),
        Some(&root_uri),
    );
    client.open_doc(&unlisted_uri, 1, unlisted_src);

    let notif1 = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("initial diagnostics");
    let diags1 = notif1.params["diagnostics"].as_array().unwrap();
    // `gnu-only-directive` (.eqv) is suppressed by `.riscv-asm-lsp.yaml`; `extension-disabled` (fadd.s on interpolated rv32imac) remains.
    assert_eq!(diags1.len(), 1, "diags1={diags1:?}");
    assert_eq!(diags1[0]["code"], json!("extension-disabled"));

    // `-Iinc` from `.riscv-asm-lsp.yaml` resolves `cfg.h` -> `KERNEL_VAL = 88`.
    let hv = hover_markdown(&mut client, &unlisted_uri, 3, 12).expect("KERNEL_VAL hover");
    assert!(hv.contains("88"), "got: {hv}");

    // Update `.riscv-asm-lsp.yaml` to replace `-march=rv32imac` with `-march=rv64gc` and notify watched-file change.
    std::fs::write(
        &yaml_path,
        "\
CompileFlags:
  Remove:
    - \"-march=*\"
  Add:
    - \"-march=rv64gc\"
    - \"-Iinc\"
Diagnostics:
  Suppress:
    - \"gnu-only-directive\"
",
    )
    .unwrap();
    client.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes": [{ "uri": yaml_uri.as_str(), "type": 2 }]
        }),
    );

    let notif2 = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("refreshed diagnostics after yaml update");
    let diags2 = notif2.params["diagnostics"].as_array().unwrap();
    assert!(
        diags2.is_empty(),
        "expected 0 diagnostics on rv64gc, got: {diags2:?}"
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}
