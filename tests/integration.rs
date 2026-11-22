mod common;

use common::{LspClient, fixtures_dir, path_to_uri};
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
            .and_then(|c| c.trigger_characters.as_ref())
            .is_some_and(|chars| chars.iter().any(|c| c == " ")),
        "expected space trigger character in completion capabilities"
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
fn hover_covers_abi_registers_pseudos_csrs_and_directives() {
    let mut client = LspClient::start(None, None);
    let path = fixtures_dir().join("kernel_entry.S");
    let text = std::fs::read_to_string(&path).expect("read kernel_entry.S");
    let uri = path_to_uri(&path);
    client.open_doc(&uri, 1, &text);

    // Line 12: `    csrr a0, mhartid`
    let hover_csr_inst = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 12, "character": 6 }
        }),
    );
    let val = hover_csr_inst.result.expect("hover csrr");
    let md = val["contents"]["value"].as_str().unwrap_or("");
    assert!(md.contains("csrr"), "expected csrr in hover: {md}");

    let hover_reg = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 12, "character": 10 }
        }),
    );
    let val = hover_reg.result.expect("hover a0");
    let md = val["contents"]["value"].as_str().unwrap_or("");
    assert!(md.contains("x10"), "expected x10 in hover: {md}");

    let hover_csr = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 12, "character": 15 }
        }),
    );
    let val = hover_csr.result.expect("hover mhartid");
    let md = val["contents"]["value"].as_str().unwrap_or("");
    assert!(md.contains("0xf14"), "expected 0xf14 in hover: {md}");

    // Line 14: `    la sp, _stack_top`
    let hover_pseudo = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 14, "character": 5 }
        }),
    );
    let md = hover_pseudo.result.expect("hover la")["contents"]["value"]
        .as_str()
        .unwrap_or("")
        .to_owned();
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
fn completion_respects_march_and_operand_position() {
    let mut client = LspClient::start(Some(json!({ "defaultMarch": "rv32i" })), None);
    let uri: lsp_types::Uri = "file:///tmp/comp_test.S".parse().expect("uri");
    client.open_doc(&uri, 1, "    \n    csrw \n");

    // Line 0 col 4: statement start under rv32i
    let resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let arr_labels = completion_labels(&resp.result);
    assert!(arr_labels.iter().any(|l| l == "addi"));
    assert!(!arr_labels.iter().any(|l| l == "mul"));

    // Line 1 col 9: `    csrw ` -> CSR operand
    let csr_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 9 }
        }),
    );
    let csr_labels = completion_labels(&csr_resp.result);
    assert!(csr_labels.iter().any(|l| l == "stvec"));
    assert!(!csr_labels.iter().any(|l| l == "a0"));

    client.shutdown_and_exit();
}

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

    // 1. Numeric labels: `1f` on line 13 -> `1:` on line 20; `1b` on line 22 -> `1:` on line 20
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

    // 2. `#define` in included header: `MSTATUS_MIE` on line 15 -> `regs.h`
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

    // 3. `.macro`: `SAVE_REG` on line 18 -> line 5; `\reg` on line 6 -> param on line 5
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

    // 4. Cross-file label: `call kernel_main` on line 19 -> `other.S`
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

    // 5. References on `1:` (line 20) includes `1:`, `1f`, `1b`
    let refs_1 = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 20, "character": 0 },
            "context": { "includeDeclaration": true }
        }),
    );
    assert_eq!(refs_1.result.as_ref().unwrap().as_array().unwrap().len(), 3);

    // 6. Rename refuses numeric labels (`1f`) and accepts ordinary labels (`trap_vector`)
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

    // 7. Document symbols list `_start` with label range ending before `trap_vector` (line 26) and child `1:`
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

    // 7b. `.Llocal_helper` in `other.S` resolves inside `other.S` (file-scoped), not `kernel_entry.S`
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

    // 7c. `.equ KERNEL_BASE` definition
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

    // 8. Workspace symbols find `kernel_main` from `other.S`
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
fn missing_tools_disable_only_tool_features_and_keep_server_alive() {
    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang-binary",
            "llvmMc": "/nonexistent/llvm-mc-binary",
            "diagnostics": { "enable": true, "delayMs": 20 }
        })),
        None,
    );
    let uri: lsp_types::Uri = "file:///tmp/missing_tools.S".parse().expect("uri");
    client.open_doc(&uri, 1, "    mv a0, a1\n    mvi a0, 1\n");

    // Hover and completion still work normally without clang/llvm-mc
    let hover = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 5 }
        }),
    );
    let md = hover.result.expect("hover")["contents"]["value"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert!(md.contains("mv"));
    assert!(!md.contains("**This line:**"));

    // No diagnostics crash even after debounce period
    std::thread::sleep(std::time::Duration::from_millis(80));
    client.shutdown_and_exit();
}

#[test]
fn unsaved_diagnostics_code_action_and_llvm_mc_hover_expansion() {
    if !has_tool("clang") || !has_tool("llvm-mc") {
        eprintln!("skipping clang/llvm-mc integration test (tools not installed)");
        return;
    }

    let mut client = LspClient::start(
        Some(json!({
            "diagnostics": { "enable": true, "delayMs": 30 }
        })),
        None,
    );

    // 1. Unsaved text with multibyte prefix before error + typo quick fix
    let err_path = fixtures_dir().join("errors.S");
    let err_uri = path_to_uri(&err_path);
    let src = "/* 日本語 */ mvi a0, 1\n    addi a0, zero, 99999999\n";
    client.open_doc(&err_uri, 1, src);

    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("expected publishDiagnostics");
    let diags = notif.params["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert_eq!(diags.len(), 2, "diagnostics: {diags:?}");
    // "/* 日本語 */ " -> 10 UTF-16 code units!
    assert_eq!(diags[0]["range"]["start"]["line"], 0);
    assert_eq!(diags[0]["range"]["start"]["character"], 10);
    assert_eq!(diags[0]["range"]["end"]["character"], 13);

    // Quick fix code action for `mvi` -> `mv`
    let ca_resp = client.send_request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": err_uri.as_str() },
            "range": diags[0]["range"],
            "context": { "diagnostics": [diags[0].clone()] }
        }),
    );
    let actions = ca_resp.result.expect("code actions");
    let arr = actions.as_array().expect("array");
    assert!(!arr.is_empty());
    assert_eq!(arr[0]["kind"], "quickfix");
    assert!(arr[0]["title"].as_str().unwrap_or("").contains("mv"));

    // 2. Hover expansion & encoding via llvm-mc
    let exp_uri: lsp_types::Uri = "file:///tmp/exp_test.S".parse().expect("uri");
    client.open_doc(
        &exp_uri,
        1,
        "    li a0, 0x12345678\n    csrw stvec, t0\n    mv a0, a1\n    la t0, sym\n",
    );

    for (line, expected_substr) in [
        (0, "lui"),
        (1, "0x10529073"),
        (2, "c.mv"),
        (3, "(relocated)"),
    ] {
        let hover = client.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": exp_uri.as_str() },
                "position": { "line": line, "character": 5 }
            }),
        );
        let md = hover.result.expect("hover")["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        assert!(
            md.contains("**This line:**") && md.contains(expected_substr),
            "line {line} missing {expected_substr}: {md}"
        );
        if line == 3 {
            assert!(!md.contains("fixup"), "fixup leaked into la hover: {md}");
            assert!(
                !md.contains("0bAAAA"),
                "placeholder leaked into la hover: {md}"
            );
        }
    }

    client.shutdown_and_exit();
}

#[test]
fn data_validation_rv64gc_instructions_assemble_with_llvm_mc() {
    if !has_tool("llvm-mc") {
        eprintln!("skipping llvm-mc data validation (llvm-mc not installed)");
        return;
    }

    use riscv_asm_lsp::data::{Database, InstructionVariant, Xlen};
    use std::io::Write;
    use std::process::{Command, Stdio};

    let db = Database::load().expect("db");
    let mut tested = 0usize;
    let mut failures = Vec::new();

    // Test every LLVM-supported rv64gc instruction/alias/pseudo variant
    let rv64gc = [
        "i", "m", "a", "f", "d", "c", "zicsr", "zifencei", "zaamo", "zalrsc", "zca", "zcd", "zmmul",
    ];

    let mut batch_lines = Vec::new();
    for inst in &db.raw.instructions {
        if !inst.llvm_supported || inst.mnemonic.starts_with(".insn") {
            continue;
        }
        let Some(variant) = inst.variants.iter().find(|v| {
            v.xlen != Xlen::Rv32
                && v.required_extensions
                    .iter()
                    .all(|req| req.split('|').any(|alt| rv64gc.contains(&alt)))
        }) else {
            continue;
        };
        if let Some(sample) = synthesize_sample_line(&inst.mnemonic, variant) {
            tested += 1;
            batch_lines.push((inst.mnemonic.clone(), sample));
        }
    }

    // Assemble in one fast pass first; if anything fails, isolate per line
    let full_input: String = batch_lines
        .iter()
        .map(|(_, line)| format!("    {line}\n"))
        .collect();
    let ok = run_llvm_mc_check("riscv64", "+m,+a,+f,+d,+c,+zicsr,+zifencei", &full_input);
    if !ok {
        for (mnem, line) in &batch_lines {
            if !run_llvm_mc_check(
                "riscv64",
                "+m,+a,+f,+d,+c,+zicsr,+zifencei",
                &format!("    {line}\n"),
            ) {
                failures.push(format!("{mnem}: `{line}`"));
            }
        }
    }

    assert!(
        tested > 200,
        "expected >200 rv64gc instructions tested, got {tested}"
    );
    assert!(
        failures.is_empty(),
        "rv64gc sample lines failed llvm-mc assembly ({}/{}): {:?}",
        failures.len(),
        tested,
        failures
    );

    fn run_llvm_mc_check(triple: &str, mattr: &str, input: &str) -> bool {
        let Ok(mut child) = Command::new("llvm-mc")
            .args([
                &format!("-triple={triple}"),
                &format!("-mattr={mattr}"),
                "-filetype=null",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    }

    fn synthesize_sample_line(mnemonic: &str, variant: &InstructionVariant) -> Option<String> {
        let mut line = variant.syntax.clone();
        // Replace longest operand names first to avoid partial collisions (`rs1` vs `rs`)
        let mut ops = variant.operands.clone();
        ops.sort_by_key(|a| std::cmp::Reverse(a.name.len()));
        for op in &ops {
            let sample = sample_for_class(&op.class)?;
            line = replace_ident(&line, &op.name, sample);
        }
        // Normalize `c.nop 0` / trailing commas from empty optional operands
        if mnemonic == "c.nop" {
            return Some("c.nop".to_owned());
        }
        Some(line)
    }

    fn replace_ident(haystack: &str, ident: &str, replacement: &str) -> String {
        let mut out = String::with_capacity(haystack.len());
        let mut i = 0;
        let bytes = haystack.as_bytes();
        while i < bytes.len() {
            if haystack[i..].starts_with(ident) {
                let before_ok =
                    i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
                let after_idx = i + ident.len();
                let after_ok = after_idx >= bytes.len()
                    || (!bytes[after_idx].is_ascii_alphanumeric() && bytes[after_idx] != b'_');
                if before_ok && after_ok {
                    out.push_str(replacement);
                    i = after_idx;
                    continue;
                }
            }
            if let Some(ch) = haystack[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                break;
            }
        }
        out
    }

    fn sample_for_class(class: &str) -> Option<&'static str> {
        match class {
            "GPR" | "GPRNoX0" | "GPRNoX0X2" | "GPRC" | "GPRMem" | "GPRCMem"
            | "GPRMemZeroOffset" | "AnyReg" | "AnyRegC" => Some("a0"),
            "GPRX1" | "GPRX1X5" => Some("ra"),
            "GPRX5" => Some("t0"),
            "SP" | "SPMem" => Some("sp"),
            "SR07" => Some("s0"),
            "FPR16" | "FPR32" | "FPR32C" | "FPR64" | "FPR64C" => Some("fa0"),
            "FPR16INX" | "FPR32INX" | "FPR64INX" | "FPR64IN32X" => Some("a0"),
            "VR" | "VRNoV0" | "VRM2" | "VRM4" | "VRM8" => Some("v8"),
            "csr_sysreg" => Some("mstatus"),
            "frmarg" | "frmarglegacy" => Some("rne"),
            "rtzarg" => Some("rtz"),
            "fencearg" => Some("iorw"),
            "bare_symbol"
            | "call_symbol"
            | "pseudo_jump_symbol"
            | "tlsdesc_call_symbol"
            | "tprel_add_symbol" => Some("sym"),
            "simm6nonzero" | "uimmlog2xlennonzero" | "c_lui_imm" => Some("1"),
            "uimm8ge32" => Some("32"),
            "simm10_lsb0000nonzero" | "uimm10_lsb00nonzero" => Some("16"),
            "simm5_plus1" => Some("1"),
            "loadfpimm" => Some("0.5"),
            "rlist" => Some("{ra}"),
            "negstackadj" => Some("-16"),
            "stackadj" => Some("16"),
            "VMaskOp" => None,
            _ => Some("0"),
        }
    }
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
fn zed_extension_queries_compile_and_match_kernel_entry() {
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let lang_dir = root.join("zed/languages/riscv_assembly");
    let language: tree_sitter::Language = tree_sitter_riscv::LANGUAGE.into();

    let mut parser = Parser::new();
    parser.set_language(&language).expect("set language");
    let kernel_src =
        std::fs::read_to_string(fixtures_dir().join("kernel_entry.S")).expect("read kernel_entry");
    let tree = parser.parse(&kernel_src, None).expect("parse kernel_entry");

    for file_name in [
        "highlights.scm",
        "injections.scm",
        "brackets.scm",
        "indents.scm",
        "outline.scm",
    ] {
        let query_src = std::fs::read_to_string(lang_dir.join(file_name)).expect("read query file");
        let query = Query::new(&language, &query_src)
            .unwrap_or_else(|err| panic!("query {file_name} failed to compile: {err}"));

        if file_name == "highlights.scm" {
            let preproc_idx = query
                .capture_index_for_name("preproc")
                .expect("@preproc capture in highlights.scm");
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), kernel_src.as_bytes());
            let mut count = 0usize;
            let mut preproc_texts = Vec::new();
            while let Some(m) = matches.next() {
                count += 1;
                for cap in m.captures() {
                    if cap.index == preproc_idx {
                        preproc_texts.push(
                            cap.node
                                .utf8_text(kernel_src.as_bytes())
                                .unwrap_or("")
                                .to_owned(),
                        );
                    }
                }
            }
            assert!(count > 20, "expected >20 highlight matches, got {count}");
            assert!(
                preproc_texts.iter().any(|t| t.contains("#include")),
                "expected #include matched as @preproc: {preproc_texts:?}"
            );
        }

        if file_name == "injections.scm" {
            let content_idx = query
                .capture_index_for_name("injection.content")
                .expect("@injection.content capture in injections.scm");
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), kernel_src.as_bytes());
            let mut injected = Vec::new();
            while let Some(m) = matches.next() {
                for cap in m.captures() {
                    if cap.index == content_idx {
                        injected.push(
                            cap.node
                                .utf8_text(kernel_src.as_bytes())
                                .unwrap_or("")
                                .to_owned(),
                        );
                    }
                }
            }
            assert!(
                injected.iter().any(|t| t.contains("#include")),
                "expected #include matched in injections.scm: {injected:?}"
            );
        }

        if file_name == "outline.scm" {
            let name_idx = query
                .capture_index_for_name("name")
                .expect("outline @name capture");
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), kernel_src.as_bytes());
            let mut names = Vec::new();
            while let Some(m) = matches.next() {
                for cap in m.captures() {
                    if cap.index == name_idx {
                        names.push(
                            cap.node
                                .utf8_text(kernel_src.as_bytes())
                                .unwrap_or("")
                                .to_owned(),
                        );
                    }
                }
            }
            assert!(
                names.iter().any(|n| n.contains("_start")),
                "outline missing _start: {names:?}"
            );
            assert!(
                names.iter().any(|n| n.contains("trap_vector")),
                "outline missing trap_vector: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "SAVE_REG"),
                "outline missing SAVE_REG macro: {names:?}"
            );
        }
    }

    let cfg_toml = std::fs::read_to_string(lang_dir.join("config.toml")).expect("read config.toml");
    assert!(cfg_toml.contains("\"riscv\""), "missing riscv suffix");
    assert!(cfg_toml.contains("block_comment = {"));
    assert!(cfg_toml.contains("documentation_comment = {"));

    let sem_rules_src = std::fs::read_to_string(lang_dir.join("semantic_token_rules.json"))
        .expect("read semantic_token_rules.json");
    let sem_rules: serde_json::Value =
        serde_json::from_str(&sem_rules_src).expect("valid semantic_token_rules.json");
    let rules = sem_rules.as_array().expect("rules array");
    for tok_type in ["register", "csr", "label", "constant", "macro", "parameter"] {
        assert!(
            rules.iter().any(|r| r["token_type"] == tok_type),
            "missing semantic_token_rules entry for {tok_type}"
        );
    }
}

fn has_tool(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn completion_labels(result: &Option<serde_json::Value>) -> Vec<String> {
    let Some(val) = result.as_ref() else {
        return Vec::new();
    };
    let arr = val
        .as_array()
        .or_else(|| val.get("items").and_then(|v| v.as_array()));
    arr.map(|a| {
        a.iter()
            .filter_map(|x| x.get("label").and_then(|l| l.as_str()).map(str::to_owned))
            .collect()
    })
    .unwrap_or_default()
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
    // Edit right in the middle of a surrogate pair emoji on line 1, plus empty line, plus out-of-range
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

    // Server must still respond cleanly to shutdown
    client.shutdown_and_exit();
}

fn completion_items_vec(result: &Option<serde_json::Value>) -> Vec<serde_json::Value> {
    let Some(val) = result.as_ref() else {
        return Vec::new();
    };
    val.as_array()
        .or_else(|| val.get("items").and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default()
}

fn hover_markdown(
    client: &mut LspClient,
    uri: &lsp_types::Uri,
    line: u32,
    character: u32,
) -> Option<String> {
    let resp = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": line, "character": character }
        }),
    );
    assert!(resp.error.is_none(), "hover error: {:?}", resp.error);
    resp.result.and_then(|v| {
        v.get("contents")
            .and_then(|c| c.get("value"))
            .and_then(|s| s.as_str())
            .map(str::to_owned)
    })
}

fn workspace_symbol_names(client: &mut LspClient, query: &str) -> Vec<String> {
    let resp = client.send_request("workspace/symbol", json!({ "query": query }));
    assert!(
        resp.error.is_none(),
        "workspace/symbol error: {:?}",
        resp.error
    );
    resp.result
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect()
}

#[test]
fn completion_provides_text_edits_relevance_docs_and_scoped_relocs() {
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("completion_edits_virtual.S"));
    let src = "\
my_sym:
    fadd.
    .op
    lui t0, %
.macro SAVE reg
    mv a0, \\
.endm
    ad
";
    client.open_doc(&uri, 1, src);

    // 1. `fadd.` -> full token replacement + filterText + sortText + documentation
    let fadd_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 9 }
        }),
    );
    let fadd_items = completion_items_vec(&fadd_resp.result);
    let fadd_s = fadd_items
        .iter()
        .find(|i| i["label"] == "fadd.s")
        .expect("fadd.s item");
    assert_eq!(fadd_s["filterText"], json!("fadd.s"));
    assert_eq!(fadd_s["textEdit"]["newText"], json!("fadd.s "));
    assert_eq!(
        fadd_s["textEdit"]["range"],
        json!({
            "start": { "line": 1, "character": 4 },
            "end": { "line": 1, "character": 9 }
        })
    );
    assert!(
        fadd_s["sortText"].as_str().is_some_and(|s| !s.is_empty()),
        "missing sortText on fadd.s"
    );
    assert!(
        !fadd_s["documentation"].is_null(),
        "missing documentation on instruction item: {fadd_s}"
    );

    // 2. `.op` -> `.option` replaces leading dot as well
    let op_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 2, "character": 7 }
        }),
    );
    let op_items = completion_items_vec(&op_resp.result);
    let opt = op_items
        .iter()
        .find(|i| i["label"] == ".option")
        .expect(".option item");
    assert_eq!(opt["filterText"], json!(".option"));
    assert_eq!(opt["textEdit"]["newText"], json!(".option "));
    assert_eq!(
        opt["textEdit"]["range"],
        json!({
            "start": { "line": 2, "character": 4 },
            "end": { "line": 2, "character": 7 }
        })
    );

    // 3. `lui t0, %` -> only relocation functions starting with `%`
    let reloc_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 3, "character": 13 }
        }),
    );
    let reloc_items = completion_items_vec(&reloc_resp.result);
    assert!(!reloc_items.is_empty());
    assert!(
        reloc_items
            .iter()
            .all(|i| i["label"].as_str().is_some_and(|s| s.starts_with('%'))),
        "unexpected non-relocation items: {reloc_items:?}"
    );
    let hi = reloc_items
        .iter()
        .find(|i| i["label"].as_str().is_some_and(|s| s.starts_with("%hi(")))
        .expect("%hi relocation");
    assert_eq!(
        hi["textEdit"]["range"],
        json!({
            "start": { "line": 3, "character": 12 },
            "end": { "line": 3, "character": 13 }
        })
    );

    // 4. `mv a0, \` inside `.macro` -> `\reg` replaces leading `\`
    let macro_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 5, "character": 12 }
        }),
    );
    let macro_items = completion_items_vec(&macro_resp.result);
    let param = macro_items
        .iter()
        .find(|i| i["label"] == "\\reg")
        .expect("\\reg item");
    assert_eq!(
        param["textEdit"]["range"],
        json!({
            "start": { "line": 5, "character": 11 },
            "end": { "line": 5, "character": 12 }
        })
    );

    // 5. `ad` -> relevance ranking (`add` sorts before `c.add` and directives like `.align`)
    let ad_resp = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 7, "character": 6 }
        }),
    );
    let ad_items = completion_items_vec(&ad_resp.result);
    let add_sort = ad_items
        .iter()
        .find(|i| i["label"] == "add")
        .and_then(|i| i["sortText"].as_str())
        .expect("add sortText");
    let c_add_sort = ad_items
        .iter()
        .find(|i| i["label"] == "c.add")
        .and_then(|i| i["sortText"].as_str())
        .expect("c.add sortText");
    let align_sort = ad_items
        .iter()
        .find(|i| i["label"] == ".align")
        .and_then(|i| i["sortText"].as_str())
        .expect(".align sortText");
    assert!(
        add_sort < c_add_sort,
        "expected add ({add_sort}) < c.add ({c_add_sort})"
    );
    assert!(
        add_sort < align_sort,
        "expected add ({add_sort}) < .align ({align_sort})"
    );

    client.shutdown_and_exit();
}

#[test]
fn hover_include_spec_syntax_and_clean_expansion() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&kernel_uri, 1, &kernel_src);

    // Item 18: Hover on `#include "regs.h"` shows resolved path
    let inc_hover = hover_markdown(&mut client, &kernel_uri, 0, 12).expect("#include hover");
    assert!(
        inc_hover.contains("regs.h"),
        "expected resolved regs.h path in hover: {inc_hover}"
    );

    // Item 18: Go to definition on `#include "regs.h"` opens regs.h
    let def_resp = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "position": { "line": 0, "character": 12 }
        }),
    );
    let def_val = def_resp.result.expect("definition result");
    let def_str = serde_json::to_string(&def_val).expect("serialize");
    assert!(
        def_str.contains("regs.h"),
        "expected definition pointing to regs.h: {def_str}"
    );

    // E1: Hover on `addi` uses structured compact layout without Spec syntax / Source trailers
    let addi_hover = hover_markdown(&mut client, &kernel_uri, 28, 4).expect("addi hover");
    assert!(
        addi_hover.starts_with("```riscv\naddi rd, rs1, imm12"),
        "expected syntax fence first in addi hover: {addi_hover}"
    );
    assert!(
        addi_hover.contains("**Add immediate**") && addi_hover.contains("RV32/RV64"),
        "expected compact metadata line in addi hover: {addi_hover}"
    );
    assert!(!addi_hover.contains("Spec syntax"));
    assert!(!addi_hover.contains("*Source:"));

    // E1: Hover layouts for alias, pseudo, CSR (list), register (one-line), directive, and project symbol
    let csrw_hover = hover_markdown(&mut client, &kernel_uri, 16, 4).expect("csrw hover");
    assert!(
        csrw_hover.contains("alias of `csrrw`"),
        "expected alias line: {csrw_hover}"
    );

    let la_pseudo = hover_markdown(&mut client, &kernel_uri, 14, 4).expect("la pseudo hover");
    assert!(
        la_pseudo.contains("pseudo-instruction"),
        "expected pseudo-instruction label: {la_pseudo}"
    );

    let stvec_hover = hover_markdown(&mut client, &kernel_uri, 16, 10).expect("stvec hover");
    assert!(
        stvec_hover.contains("`stvec` · 0x105 · S-mode"),
        "expected compact CSR header: {stvec_hover}"
    );
    assert!(
        stvec_hover.contains("- `MODE` [1:0]"),
        "expected field bullet list: {stvec_hover}"
    );
    assert!(!stvec_hover.contains("| Bits |"));

    let sp_hover = hover_markdown(&mut client, &kernel_uri, 14, 7).expect("sp hover");
    assert!(
        !sp_hover.contains('\n'),
        "register hover must be a single line: {sp_hover}"
    );
    assert!(
        sp_hover.contains("`sp` (x2)") && sp_hover.contains("callee-saved"),
        "expected compact register hover: {sp_hover}"
    );

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

    // Item 6: Hover on `ld` must not report self-referencing alias
    let ld_hover = hover_markdown(&mut client, &kernel_uri, 30, 4).expect("ld hover");
    assert!(
        !ld_hover.contains("alias of `ld`"),
        "unexpected self-alias in ld hover: {ld_hover}"
    );

    if has_tool("llvm-mc") {
        // R1 + E1: `la` expansion filters `.Lpcrel_hi0:`, `.text`, `#   fixup`, and formats `(relocated)`
        let la_hover = hover_markdown(&mut client, &kernel_uri, 14, 4).expect("la hover");
        let (_, exp_section) = la_hover
            .split_once("**This line:**")
            .expect("this-line section");
        assert!(exp_section.contains("auipc"), "exp: {exp_section}");
        assert!(exp_section.contains("(relocated)"), "exp: {exp_section}");
        assert!(
            !exp_section.contains("fixup"),
            "fixup leaked into expansion: {exp_section}"
        );
        assert!(
            !exp_section.contains("0bAAAA"),
            "placeholder leaked into expansion: {exp_section}"
        );
        assert!(
            !exp_section.contains(".Lpcrel_hi0:"),
            "label leaked into expansion: {exp_section}"
        );
        assert!(
            !exp_section.contains(".text"),
            "directive leaked into expansion: {exp_section}"
        );

        // E1: `addi sp, sp, -16` formats compressed hex word `0x1141`
        let addi_exp = hover_markdown(&mut client, &kernel_uri, 28, 4).expect("addi exp");
        assert!(
            addi_exp.contains("**This line:** `c.addi sp, -16` · `0x1141`"),
            "expected hex word encoding in addi hover: {addi_exp}"
        );

        // Item 7: `ret` after `.option` must filter `.option` lines from expansion
        let opt_uri = path_to_uri(&fixtures.join("virtual_option_ret.S"));
        client.open_doc(&opt_uri, 1, ".option norvc\n.option rvc\n    ret\n");
        let ret_hover = hover_markdown(&mut client, &opt_uri, 2, 4).expect("ret hover");
        let (_, ret_exp) = ret_hover
            .split_once("**This line:**")
            .expect("ret this-line section");
        assert!(
            ret_exp.contains("c.jr") || ret_exp.contains("jalr"),
            "ret exp: {ret_exp}"
        );
        assert!(
            !ret_exp.contains(".option"),
            ".option leaked into expansion: {ret_exp}"
        );
    }

    if has_tool("llvm-mc") && has_tool("clang") {
        // Item 8: `li t0, MSTATUS_MIE` expands `#define` constant via index / clang -E
        let li_hover = hover_markdown(&mut client, &kernel_uri, 15, 4).expect("li hover");
        assert!(
            li_hover.contains("addi") || li_hover.contains("c.li"),
            "expected expanded li instruction for MSTATUS_MIE: {li_hover}"
        );
        assert!(
            !li_hover.contains("expansion unavailable"),
            "MSTATUS_MIE should expand cleanly: {li_hover}"
        );

        // Item 8: Unknown constant shows explicit unavailable notice rather than omitting silently
        let unexp_uri = path_to_uri(&fixtures.join("virtual_unexpandable.S"));
        client.open_doc(&unexp_uri, 1, "    li a0, unknown_external_sym\n");
        let unexp_hover = hover_markdown(&mut client, &unexp_uri, 0, 4).expect("unexp hover");
        assert!(
            unexp_hover.contains("expansion unavailable"),
            "expected unavailable notice: {unexp_hover}"
        );
    }

    client.shutdown_and_exit();
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

    // Update a.S and create b.S on disk without opening either in editor
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
    assert!(
        after_update.iter().any(|s| s == "updated_watched_sym"),
        "updated symbol missing: {after_update:?}"
    );
    assert!(
        after_update.iter().any(|s| s == "created_watched_sym"),
        "created symbol missing: {after_update:?}"
    );
    assert!(
        !after_update.iter().any(|s| s == "initial_watched_sym"),
        "stale symbol remained: {after_update:?}"
    );

    // Delete b.S on disk and notify server
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
    assert!(
        after_delete.is_empty(),
        "deleted symbol remained: {after_delete:?}"
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn completion_snippets_label_details_and_trailing_separators() {
    // 1. Client with snippetSupport + labelDetailsSupport (E2)
    let mut snip_client = LspClient::start_with_snippets(None, None);
    let uri = path_to_uri(&fixtures_dir().join("completion_snip_virtual.S"));
    snip_client.open_doc(&uri, 1, "    \n    .\n");

    let stmt_resp = snip_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let stmt_items = completion_items_vec(&stmt_resp.result);

    let addi = stmt_items
        .iter()
        .find(|i| i["label"] == "addi")
        .expect("addi snippet");
    assert_eq!(addi["insertTextFormat"], json!(2));
    assert_eq!(addi["labelDetails"]["detail"], json!(" rd, rs1, imm12"));
    assert_eq!(
        addi["textEdit"]["newText"],
        json!("addi ${1:rd}, ${2:rs1}, ${3:imm12}$0")
    );

    let ld_items: Vec<_> = stmt_items.iter().filter(|i| i["label"] == "ld").collect();
    assert!(ld_items.len() >= 2);
    assert_eq!(
        ld_items[0]["textEdit"]["newText"],
        json!("ld ${1:rd}, ${2:0}(${3:rs1})$0")
    );

    let csrw_items: Vec<_> = stmt_items.iter().filter(|i| i["label"] == "csrw").collect();
    assert_eq!(csrw_items.len(), 2);
    assert_eq!(csrw_items[0]["filterText"], json!("csrw"));
    assert_eq!(csrw_items[1]["filterText"], json!("csrw"));
    assert!(
        csrw_items[0]["sortText"].as_str().unwrap_or("")
            < csrw_items[1]["sortText"].as_str().unwrap_or("")
    );
    assert_eq!(csrw_items[0]["labelDetails"]["detail"], json!(" csr, rs"));
    assert_eq!(
        csrw_items[0]["textEdit"]["newText"],
        json!("csrw ${1:csr}, ${2:rs}$0")
    );

    let ret = stmt_items
        .iter()
        .find(|i| i["label"] == "ret")
        .expect("ret plain");
    assert_eq!(ret["insertTextFormat"], json!(1));
    assert_eq!(ret["textEdit"]["newText"], json!("ret"));

    let dir_resp = snip_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 5 }
        }),
    );
    let dir_items = completion_items_vec(&dir_resp.result);
    let mac = dir_items
        .iter()
        .find(|i| i["label"] == ".macro")
        .expect(".macro snippet");
    assert_eq!(
        mac["textEdit"]["newText"],
        json!(".macro ${1:name} ${2:args}\n\t$0\n.endm")
    );
    let opt_items: Vec<_> = dir_items
        .iter()
        .filter(|i| i["label"] == ".option")
        .collect();
    assert_eq!(opt_items.len(), 2);
    assert_eq!(
        opt_items[0]["textEdit"]["newText"],
        json!(".option push\n.option ${1:norvc}\n$0\n.option pop")
    );
    snip_client.shutdown_and_exit();

    // 2. Plain client without snippetSupport (E3 trailing separators)
    let mut plain_client = LspClient::start(None, None);
    let plain_uri = path_to_uri(&fixtures_dir().join("completion_plain_virtual.S"));
    plain_client.open_doc(
        &plain_uri,
        1,
        "lbl:\n    addi\n    .option \n    addi a\n    addi a0, a1, l\n",
    );

    let p1 = plain_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": plain_uri.as_str() },
            "position": { "line": 1, "character": 8 }
        }),
    );
    let p1_items = completion_items_vec(&p1.result);
    assert_eq!(
        p1_items
            .iter()
            .find(|i| i["label"] == "addi")
            .expect("addi")["textEdit"]["newText"],
        json!("addi ")
    );
    assert_eq!(
        p1_items.iter().find(|i| i["label"] == "ret").expect("ret")["textEdit"]["newText"],
        json!("ret")
    );
    assert_eq!(
        p1_items
            .iter()
            .find(|i| i["label"] == ".globl")
            .expect(".globl")["textEdit"]["newText"],
        json!(".globl ")
    );
    assert_eq!(
        p1_items
            .iter()
            .find(|i| i["label"] == ".text")
            .expect(".text")["textEdit"]["newText"],
        json!(".text")
    );

    let p2 = plain_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": plain_uri.as_str() },
            "position": { "line": 2, "character": 12 }
        }),
    );
    let p2_items = completion_items_vec(&p2.result);
    assert_eq!(
        p2_items
            .iter()
            .find(|i| i["label"] == "arch")
            .expect("arch")["textEdit"]["newText"],
        json!("arch, ")
    );
    assert_eq!(
        p2_items.iter().find(|i| i["label"] == "rvc").expect("rvc")["textEdit"]["newText"],
        json!("rvc")
    );

    let p3 = plain_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": plain_uri.as_str() },
            "position": { "line": 3, "character": 10 }
        }),
    );
    let p3_items = completion_items_vec(&p3.result);
    assert_eq!(
        p3_items.iter().find(|i| i["label"] == "a0").expect("a0")["textEdit"]["newText"],
        json!("a0, ")
    );

    let p4 = plain_client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": plain_uri.as_str() },
            "position": { "line": 4, "character": 16 }
        }),
    );
    let p4_items = completion_items_vec(&p4.result);
    assert_eq!(
        p4_items.iter().find(|i| i["label"] == "lbl").expect("lbl")["textEdit"]["newText"],
        json!("lbl")
    );

    plain_client.shutdown_and_exit();
}

#[test]
fn semantic_tokens_full_and_range_classify_arch_and_project_symbols() {
    let fixtures = fixtures_dir();
    let root_uri = path_to_uri(&fixtures);
    let kernel_path = fixtures.join("kernel_entry.S");
    let kernel_uri = path_to_uri(&kernel_path);
    let kernel_src = std::fs::read_to_string(&kernel_path).expect("read kernel_entry.S");

    let mut client = LspClient::start(None, Some(&root_uri));
    client.open_doc(&kernel_uri, 1, &kernel_src);

    let legend = client
        .init_result
        .capabilities
        .semantic_tokens_provider
        .as_ref()
        .expect("semantic tokens capability");
    let token_types: Vec<String> = match legend {
        lsp_types::SemanticTokensServerCapabilities::SemanticTokensOptions(o) => o
            .legend
            .token_types
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect(),
        lsp_types::SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(o) => o
            .semantic_tokens_options
            .legend
            .token_types
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect(),
    };
    let type_idx = |name: &str| {
        token_types
            .iter()
            .position(|t| t == name)
            .expect("legend type") as u32
    };

    let full_resp = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": kernel_uri.as_str() } }),
    );
    let full_data = full_resp.result.expect("semanticTokens/full")["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect::<Vec<_>>();
    let decode = |raw: &[u32]| -> Vec<(String, u32, u32)> {
        let lines: Vec<&str> = kernel_src.lines().collect();
        let mut out = Vec::new();
        let mut line = 0u32;
        let mut col = 0u32;
        for chunk in raw.as_chunks::<5>().0 {
            line += chunk[0];
            col = if chunk[0] == 0 {
                col + chunk[1]
            } else {
                chunk[1]
            };
            let len = chunk[2];
            let utf16: Vec<u16> = lines[line as usize].encode_utf16().collect();
            let lex = String::from_utf16_lossy(&utf16[col as usize..(col + len) as usize]);
            out.push((lex, chunk[3], chunk[4]));
        }
        out
    };

    let decoded = decode(&full_data);
    assert!(decoded.contains(&("KERNEL_BASE".to_owned(), type_idx("constant"), 1)));
    assert!(decoded.contains(&("SAVE_REG".to_owned(), type_idx("macro"), 1)));
    assert!(decoded.contains(&("reg".to_owned(), type_idx("parameter"), 1)));
    assert!(decoded.contains(&("\\reg".to_owned(), type_idx("parameter"), 0)));
    assert!(decoded.contains(&("_start".to_owned(), type_idx("label"), 1)));
    assert!(decoded.contains(&("mhartid".to_owned(), type_idx("csr"), 0)));
    assert!(decoded.contains(&("stvec".to_owned(), type_idx("csr"), 0)));
    assert!(decoded.contains(&("MSTATUS_MIE".to_owned(), type_idx("constant"), 0)));
    assert!(decoded.contains(&("t0".to_owned(), type_idx("register"), 0)));
    assert!(decoded.contains(&("SAVE_REG".to_owned(), type_idx("macro"), 0)));
    assert!(decoded.contains(&("1".to_owned(), type_idx("label"), 1)));
    assert!(decoded.contains(&("1f".to_owned(), type_idx("label"), 0)));
    assert!(decoded.contains(&("1b".to_owned(), type_idx("label"), 0)));

    // Range request limited to lines 12..=16
    let range_resp = client.send_request(
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": { "uri": kernel_uri.as_str() },
            "range": {
                "start": { "line": 12, "character": 0 },
                "end": { "line": 16, "character": 30 }
            }
        }),
    );
    let range_data = range_resp.result.expect("semanticTokens/range")["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect::<Vec<_>>();
    let range_decoded = decode(&range_data);
    assert!(range_decoded.contains(&("mhartid".to_owned(), type_idx("csr"), 0)));
    assert!(range_decoded.contains(&("stvec".to_owned(), type_idx("csr"), 0)));
    assert!(!range_decoded.contains(&("KERNEL_BASE".to_owned(), type_idx("constant"), 1)));
    assert!(!range_decoded.contains(&("trap_vector".to_owned(), type_idx("label"), 1)));

    client.shutdown_and_exit();
}
