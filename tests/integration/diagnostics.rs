use crate::common::{LspClient, fixtures_dir, has_tool, hover_markdown, path_to_uri};
use serde_json::json;

#[test]
fn missing_tools_disable_only_tool_features_and_keep_server_alive() {
    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang-binary",
            "llvmMc": "/nonexistent/llvm-mc-binary",
            "diagnostics": { "enable": true, "delayMs": 20, "external": "on" }
        })),
        None,
    );
    let uri = crate::common::test_uri("missing_tools.S");
    client.open_doc(&uri, 1, "    mv a0, a1\n    mvi a0, 1\n");

    let md = hover_markdown(&mut client, &uri, 0, 5).expect("hover");
    assert!(md.contains("mv"));
    assert!(!md.contains("**This line:**"));

    let msg = client
        .wait_for_notification("window/showMessage", std::time::Duration::from_secs(2))
        .expect("expected showMessage for missing tool");
    assert_eq!(msg.params["type"], json!(2));
    let txt = msg.params["message"].as_str().unwrap_or("");
    assert!(
        txt.contains("#optional-llvm-for-clang-based-diagnostics-and-expansions")
            && (txt.contains("settings.clang") || txt.contains("settings.llvmMc"))
            && !txt.contains("LLVM 19"),
        "unexpected showMessage text: {txt}"
    );

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires clang"]
fn unsaved_diagnostics_and_code_actions_with_clang() {
    assert!(
        has_tool("clang"),
        "clang must be on PATH for ignored tool tests"
    );

    let mut client = LspClient::start(
        Some(json!({
            "diagnostics": { "enable": true, "delayMs": 30, "external": "on" }
        })),
        None,
    );

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

    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires clang"]
fn macro_and_included_file_diagnostics_and_compile_working_dir() {
    assert!(
        has_tool("clang"),
        "clang must be on PATH for ignored tool tests"
    );

    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_inc_diag_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sub_dir = tmp.join("sub_build");
    std::fs::create_dir_all(&sub_dir).unwrap();

    // Case 1 (Item 2): macro instantiation error + `.include "bad.inc"`
    std::fs::write(tmp.join("bad.inc"), "    mov a0, a1\n").unwrap();
    let main_s = tmp.join("main.S");
    let main_src = "\
    .include \"bad.inc\"
.macro TWICE
    nop
    mvx a0, a1
.endm
    TWICE
";
    std::fs::write(&main_s, main_src).unwrap();

    let root_uri = path_to_uri(&tmp);
    let main_uri = path_to_uri(&main_s);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20, "external": "on" } })),
        Some(&root_uri),
    );
    client.open_doc(&main_uri, 1, main_src);

    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("expected publishDiagnostics");
    let diags = notif.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 2, "diags: {diags:?}");
    assert_eq!(diags[0]["range"]["start"]["line"], 0);
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("bad.inc:1:"),
        "diag0: {:?}",
        diags[0]
    );
    assert!(diags[0]["relatedInformation"].is_array());
    assert_eq!(diags[1]["range"]["start"]["line"], 5);
    assert!(
        diags[1]["message"]
            .as_str()
            .unwrap_or("")
            .contains("in macro instantiation"),
        "diag1: {:?}",
        diags[1]
    );
    client.shutdown_and_exit();

    // Case 2 (Item 3): `compile_commands.json` sets `"directory"` to `sub_dir`, allowing
    // `.include "from_cwd.inc"` without `-I` when clang runs in `current_dir = directory`.
    std::fs::write(sub_dir.join("from_cwd.inc"), "    nop\n").unwrap();
    let sub_main = tmp.join("cwd_test.S");
    let sub_src = "    .include \"from_cwd.inc\"\n";
    std::fs::write(&sub_main, sub_src).unwrap();
    std::fs::write(
        tmp.join("compile_commands.json"),
        serde_json::to_string_pretty(&json!([{
            "directory": sub_dir.to_string_lossy(),
            "command": format!("clang --target=riscv64-unknown-elf -march=rv64gc -c {}", sub_main.display()),
            "file": sub_main.to_string_lossy()
        }]))
        .unwrap(),
    )
    .unwrap();

    let mut client2 = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20, "external": "on" } })),
        Some(&root_uri),
    );
    let sub_uri = path_to_uri(&sub_main);
    client2.open_doc(&sub_uri, 1, sub_src);
    let notif2 = client2
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("expected publishDiagnostics for cwd test");
    let diags2 = notif2.params["diagnostics"].as_array().unwrap();
    assert!(
        diags2.is_empty(),
        "expected 0 diags using cwd, got {diags2:?}"
    );

    // Case 3 (Item 5): missing `.include "not_there.inc"` spans the quoted filename.
    let miss_src = "    .include \"not_there.inc\"\n";
    client2.open_doc(&sub_uri, 2, miss_src);
    let notif3 = client2
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("expected publishDiagnostics for missing include");
    let diags3 = notif3.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags3.len(), 1, "diags3: {diags3:?}");
    assert_eq!(
        diags3[0]["range"]["start"],
        json!({ "line": 0, "character": 13 })
    );
    assert_eq!(
        diags3[0]["range"]["end"],
        json!({ "line": 0, "character": 28 })
    );

    // Case 4: multibyte prefix in included file converts `relatedInformation` column to UTF-16
    // and `-include-pch` flag in compile_commands.json is ignored rather than mangled.
    std::fs::write(sub_dir.join("mb.inc"), "/* 日本語 */ mov a0, a1\n").unwrap();
    let mb_main = tmp.join("mb_test.S");
    let mb_src = "    .include \"mb.inc\"\n";
    std::fs::write(&mb_main, mb_src).unwrap();
    std::fs::write(
        tmp.join("compile_commands.json"),
        serde_json::to_string_pretty(&json!([
            {
                "directory": sub_dir.to_string_lossy(),
                "arguments": ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-include-pch", "unused.pch", "-c", mb_main.to_string_lossy()],
                "file": mb_main.to_string_lossy()
            }
        ]))
        .unwrap(),
    )
    .unwrap();
    client2.shutdown_and_exit();

    let mut client3 = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20, "external": "on" } })),
        Some(&root_uri),
    );
    let mb_uri = path_to_uri(&mb_main);
    client3.open_doc(&mb_uri, 1, mb_src);
    let notif4 = client3
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("expected publishDiagnostics for mb_test");
    let diags4 = notif4.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags4.len(), 1, "diags4: {diags4:?}");
    assert_eq!(
        diags4[0]["relatedInformation"][0]["location"]["range"]["start"],
        json!({ "line": 0, "character": 10 })
    );

    client3.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "requires llvm-mc"]
fn data_validation_rv64gc_instructions_assemble_with_llvm_mc() {
    assert!(
        has_tool("llvm-mc"),
        "llvm-mc must be on PATH for ignored tool tests"
    );

    use riscv_asm_lsp::data::{Database, InstructionVariant, Xlen};
    use std::io::Write;
    use std::process::{Command, Stdio};

    let db = Database::load().expect("db");
    let mut tested = 0usize;
    let mut failures = Vec::new();

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
        let mut ops = variant.operands.clone();
        ops.sort_by_key(|a| std::cmp::Reverse(a.name.len()));
        for op in &ops {
            let sample = sample_for_class(&op.class)?;
            line = replace_ident(&line, &op.name, sample);
        }
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
            "GPR" | "GPRNoX0" | "GPRNoX2" | "GPRNoX0X2" | "GPRC" | "GPRMem" | "GPRCMem"
            | "BasePtr" | "BasePtrC" | "GPRMemZeroOffset" | "YGPR" | "AnyReg" | "AnyRegC" => {
                Some("a0")
            }
            "GPRX1" | "GPRX1X5" => Some("ra"),
            "GPRX5" => Some("t0"),
            "SP" | "SPMem" => Some("sp"),
            "SR07" => Some("s0"),
            "FPR16" | "FPR32" | "FPR32C" | "FPR64" | "FPR64C" | "FPR128" => Some("fa0"),
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
            "rlist" | "reglist" => Some("{ra}"),
            "negstackadj" => Some("-16"),
            "stackadj" => Some("16"),
            "VMaskOp" => None,
            _ => Some("0"),
        }
    }
}

#[test]
fn code_actions_offer_use_slash_comment_only_when_preprocessed() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_ca_pp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let pre_s = tmp.join("pre.S");
    let raw_s = tmp.join("raw.s");
    let forced_s = tmp.join("forced.s");
    let src = "\
# just a comment
    # include this later
# define X 1
_start:
    nop
";
    std::fs::write(&pre_s, src).unwrap();
    std::fs::write(&raw_s, src).unwrap();
    std::fs::write(&forced_s, src).unwrap();
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
    let pre_uri = path_to_uri(&pre_s);
    let raw_uri = path_to_uri(&raw_s);
    let forced_uri = path_to_uri(&forced_s);

    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": false } })),
        Some(&root_uri),
    );
    client.open_doc(&pre_uri, 1, src);
    client.open_doc(&raw_uri, 1, src);
    client.open_doc(&forced_uri, 1, src);

    for uri in [&pre_uri, &forced_uri] {
        // Line 0 (`# just a comment`) is a pure comment -> non-diagnostic refactor offer.
        let ca0 = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri.as_str() },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        let arr = ca0
            .result
            .expect("code actions")
            .as_array()
            .unwrap()
            .clone();
        let action = arr
            .iter()
            .find(|a| a["title"] == "Use `//` comment")
            .expect("Use // comment action");
        let edits = action["edit"]["changes"][uri.as_str()]
            .as_array()
            .expect("edits");
        assert_eq!(edits[0]["newText"], json!("//"));
        assert_eq!(
            edits[0]["range"],
            json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            })
        );

        // Lines 1 and 2 (`# include ...`, `# define X 1`) look like preprocessor directives and
        // have no diagnostic here -> do not offer a `//` refactor action unprompted.
        for (line, col) in [(1u32, 4u32), (2u32, 0u32)] {
            let ca = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri.as_str() },
                    "range": {
                        "start": { "line": line, "character": col },
                        "end": { "line": line, "character": col }
                    },
                    "context": { "diagnostics": [] }
                }),
            );
            assert!(
                ca.result.as_ref().is_none_or(|v| v.is_null()),
                "unexpected refactor action on preprocessor-like line {line}: {ca:?}"
            );
        }

        // Line 4 (`nop`) is not a `#` line -> null
        let ca_nop = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri.as_str() },
                "range": {
                    "start": { "line": 4, "character": 4 },
                    "end": { "line": 4, "character": 4 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        assert!(ca_nop.result.as_ref().is_none_or(|v| v.is_null()));
    }

    // `.s` without `-x assembler-with-cpp`: `#` is already a valid comment -> no action
    for line in [0u32, 1, 2] {
        let ca = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": raw_uri.as_str() },
                "range": {
                    "start": { "line": line, "character": 0 },
                    "end": { "line": line, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        assert!(
            ca.result.as_ref().is_none_or(|v| v.is_null()),
            "unexpected action on .s line {line}: {:?}",
            ca.result
        );
    }

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "requires clang"]
fn preprocessor_hash_lines_table_and_x_override_with_clang() {
    assert!(
        has_tool("clang"),
        "clang must be on PATH for ignored tool tests"
    );

    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_l1_clang_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let safe_s = tmp.join("safe.S");
    let err_s = tmp.join("err.S");
    let spaced_s = tmp.join("spaced.S");
    let plain_s = tmp.join("plain.s");
    let forced_s = tmp.join("forced.s");

    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([{
            "directory": tmp.to_string_lossy(),
            "arguments": ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-x", "assembler-with-cpp", "-c", forced_s.to_string_lossy()],
            "file": forced_s.to_string_lossy()
        }])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let safe_uri = path_to_uri(&safe_s);
    let err_uri = path_to_uri(&err_s);
    let spaced_uri = path_to_uri(&spaced_s);
    let plain_uri = path_to_uri(&plain_s);
    let forced_uri = path_to_uri(&forced_s);

    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 20, "external": "on" } })),
        Some(&root_uri),
    );

    // 1. Safe `#` forms in `.S`: `# just a comment`, `nop # if define error`, `#`, `# pragma: keep`, `# 100 note`
    let safe_src = "\
# just a comment
#
# pragma: keep
# 100 note
_start:
    nop # if define error
";
    client.open_doc(&safe_uri, 1, safe_src);
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("safe.S publishDiagnostics");
    assert_eq!(notif.params["uri"], json!(safe_uri.as_str()));
    assert_eq!(notif.params["diagnostics"], json!([]));

    // 2. Failing `#` forms in `.S`: `# if ...`, `    # include ...`, `# error ...` + QuickFix
    let err_src = "\
    # include this later
# error handling starts here
# if the value is zero, skip
";
    client.open_doc(&err_uri, 1, err_src);
    let notif_err = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("err.S publishDiagnostics");
    assert_eq!(notif_err.params["uri"], json!(err_uri.as_str()));
    let diags_err = notif_err.params["diagnostics"].as_array().unwrap();
    for line in [0u64, 1, 2] {
        let diag = diags_err
            .iter()
            .find(|d| d["range"]["start"]["line"] == line)
            .unwrap_or_else(|| panic!("missing diagnostic on line {line}: {diags_err:?}"));
        let ca = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": err_uri.as_str() },
                "range": diag["range"],
                "context": { "diagnostics": [diag.clone()] }
            }),
        );
        let arr = ca.result.expect("quickfixes").as_array().unwrap().clone();
        assert!(
            arr.iter()
                .any(|a| a["title"] == "Use `//` comment" && a["kind"] == "quickfix"),
            "missing Use // comment quickfix on line {line}: {arr:?}"
        );
    }

    // 3. `# define X 5 is the size` then `li a0, X` -> no diag on line 0, error on line 2 with relatedInformation + quickfix
    let spaced_src = "\
# define X 5 is the size
_start:
    li a0, X
";
    client.open_doc(&spaced_uri, 1, spaced_src);
    let notif_spaced = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("spaced.S publishDiagnostics");
    assert_eq!(notif_spaced.params["uri"], json!(spaced_uri.as_str()));
    let diags_spaced = notif_spaced.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags_spaced.len(), 1, "diags: {diags_spaced:?}");
    assert_eq!(diags_spaced[0]["range"]["start"]["line"], 2);
    let rel = diags_spaced[0]["relatedInformation"]
        .as_array()
        .expect("relatedInformation");
    assert_eq!(rel[0]["location"]["range"]["start"]["line"], 0);
    assert!(
        rel[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not a comment")
    );
    let ca_spaced = client.send_request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": spaced_uri.as_str() },
            "range": diags_spaced[0]["range"],
            "context": { "diagnostics": [diags_spaced[0].clone()] }
        }),
    );
    let spaced_actions = ca_spaced
        .result
        .expect("spaced quickfix")
        .as_array()
        .unwrap()
        .clone();
    let fix_action = spaced_actions
        .iter()
        .find(|a| a["kind"] == "quickfix" && a["title"].as_str().unwrap_or("").contains("line 1"))
        .expect("quickfix pointing to line 1");
    let fix_edit = &fix_action["edit"]["changes"][spaced_uri.as_str()][0];
    assert_eq!(fix_edit["range"]["start"]["line"], 0);

    // 4. `.s` without `-x assembler-with-cpp`: `#define A 1` is a comment, so `li a0, A` errors
    let def_src = "#define A 1\n_start:\n    li a0, A\n";
    client.open_doc(&plain_uri, 1, def_src);
    let notif_plain = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("plain.s publishDiagnostics");
    assert_eq!(notif_plain.params["uri"], json!(plain_uri.as_str()));
    let diags_plain = notif_plain.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags_plain.len(), 1, "plain.s diags: {diags_plain:?}");
    assert_eq!(diags_plain[0]["range"]["start"]["line"], 2);
    assert!(diags_plain[0].get("relatedInformation").is_none());

    // 5. `.s` with `compile_commands.json` `-x assembler-with-cpp`: `#define A 1` succeeds with 0 diags
    client.open_doc(&forced_uri, 1, def_src);
    let notif_forced = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("forced.s publishDiagnostics");
    assert_eq!(notif_forced.params["uri"], json!(forced_uri.as_str()));
    assert_eq!(notif_forced.params["diagnostics"], json!([]));

    // 6. Linux-style `# define CZ ctz` with Zbb error: neutral relatedInfo, NO `Use // comment` quickfix
    let linux_uri = path_to_uri(&tmp.join("linux.S"));
    let linux_src = "\
#ifdef BIG
# define CZ clz
#else
# define CZ ctz
#endif
    CZ a0, a1
";
    client.open_doc(&linux_uri, 1, linux_src);
    let notif_linux = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(5),
        )
        .expect("linux.S publishDiagnostics");
    assert_eq!(notif_linux.params["uri"], json!(linux_uri.as_str()));
    let diags_linux = notif_linux.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags_linux.len(), 1, "linux.S diags: {diags_linux:?}");
    assert_eq!(diags_linux[0]["range"]["start"]["line"], 5);
    let rel_linux = diags_linux[0]["relatedInformation"]
        .as_array()
        .expect("neutral relatedInformation");
    assert_eq!(rel_linux[0]["location"]["range"]["start"]["line"], 3);
    assert_eq!(
        rel_linux[0]["message"],
        json!("`CZ` expands to `ctz` (defined on line 4)")
    );
    let ca_linux = client.send_request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": linux_uri.as_str() },
            "range": diags_linux[0]["range"],
            "context": { "diagnostics": [diags_linux[0].clone()] }
        }),
    );
    let has_comment_fix = ca_linux
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|a| {
                a["title"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Use `//` comment")
            })
        });
    assert!(
        !has_comment_fix,
        "unexpected `Use // comment` quickfix for Linux-style macro: {:?}",
        ca_linux.result
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn fake_clang_discovery_version_independent_and_warns_only_when_configured() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_fake_llvm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let bin_v18 = tmp.join("bin_v18");
    let bin_fallback = tmp.join("bin_fallback");
    let bin_no_rv = tmp.join("bin_no_rv");
    std::fs::create_dir_all(&bin_v18).unwrap();
    std::fs::create_dir_all(&bin_fallback).unwrap();
    std::fs::create_dir_all(&bin_no_rv).unwrap();

    let write_exe = |path: &std::path::Path, body: &str| {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };

    write_exe(
        &bin_v18.join("clang"),
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'clang version 18.1.3'; exit 0 ;;\n-print-targets) printf '  Registered Targets:\\n    riscv64 - 64-bit RISC-V\\n'; exit 0 ;;\nesac\ncat >/dev/null\necho '<stdin>:1:5: error: fake_v18_diag' >&2\nexit 1\n",
    );

    let doc_path = tmp.join("test.S");
    let uri = path_to_uri(&doc_path);

    // 1. Auto-discovered clang of another LLVM major version works with zero showMessage warnings
    let mut client18 = LspClient::start_with_env(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 15, "external": "auto" } })),
        None,
        &[("PATH", bin_v18.to_str().unwrap())],
    );
    client18.open_doc(&uri, 1, "    mvi a0, 1\n");

    let diag1 = client18
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("diagnostics enabled on LLVM 18");
    assert!(
        diag1.params["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("fake_v18_diag")
    );
    assert!(
        client18
            .wait_for_notification("window/showMessage", std::time::Duration::from_millis(80))
            .is_none(),
        "auto-discovered LLVM 18 must not send a version warning"
    );
    client18.shutdown_and_exit();

    // 2. Auto-discovered bare clang without RISC-V target falls through to clang-20 with RISC-V
    write_exe(
        &bin_fallback.join("clang"),
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'Apple clang version 16.0.0'; exit 0 ;;\n-print-targets) printf '  Registered Targets:\\n    x86-64 - 64-bit X86\\n'; exit 0 ;;\nesac\nexit 1\n",
    );
    write_exe(
        &bin_fallback.join("clang-20"),
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'clang version 20.1.0'; exit 0 ;;\n-print-targets) printf '  Registered Targets:\\n    riscv64 - 64-bit RISC-V\\n'; exit 0 ;;\nesac\ncat >/dev/null\necho '<stdin>:1:5: error: fake_v20_diag' >&2\nexit 1\n",
    );

    let mut client_fb = LspClient::start_with_env(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 15, "external": "auto" } })),
        None,
        &[("PATH", bin_fallback.to_str().unwrap())],
    );
    client_fb.open_doc(&uri, 1, "    mvi a0, 1\n");

    let diag_fb = client_fb
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("fallback clang-20 diagnostics");
    assert!(
        diag_fb.params["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("fake_v20_diag")
    );
    assert!(
        client_fb
            .wait_for_notification("window/showMessage", std::time::Duration::from_millis(80))
            .is_none(),
        "fallback auto-discovery must not send showMessage"
    );
    client_fb.shutdown_and_exit();

    // 3. Auto-discovered clang without RISC-V and no usable fallback disables diagnostics quietly
    let clang_no_rv = bin_no_rv.join("clang");
    write_exe(
        &clang_no_rv,
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'Apple clang version 16.0.0'; exit 0 ;;\n-print-targets) printf '  Registered Targets:\\n    x86-64 - 64-bit X86\\n'; exit 0 ;;\nesac\nexit 1\n",
    );

    let mut client_auto_norv = LspClient::start_with_env(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 15, "external": "auto" } })),
        None,
        &[("PATH", bin_no_rv.to_str().unwrap())],
    );
    client_auto_norv.open_doc(&uri, 1, "    addi a0, a1, 1\n");
    assert!(
        client_auto_norv
            .wait_for_notification("window/showMessage", std::time::Duration::from_millis(120))
            .is_none(),
        "auto-discovery without RISC-V tool must stay silent in showMessage"
    );
    let hv_auto = hover_markdown(&mut client_auto_norv, &uri, 0, 5).expect("hover keeps working");
    assert!(hv_auto.contains("Add immediate"), "got: {hv_auto}");
    client_auto_norv.shutdown_and_exit();

    // 4. Explicitly configured clang without RISC-V target disables diagnostics and warns once
    let mut client_cfg_norv = LspClient::start_with_env(
        Some(json!({
            "clang": clang_no_rv.to_str().unwrap(),
            "diagnostics": { "enable": true, "delayMs": 15, "external": "auto" }
        })),
        None,
        &[("PATH", bin_no_rv.to_str().unwrap())],
    );
    client_cfg_norv.open_doc(&uri, 1, "    addi a0, a1, 1\n");

    let show_norv = client_cfg_norv
        .wait_for_notification("window/showMessage", std::time::Duration::from_secs(3))
        .expect("configured missing riscv target showMessage");
    assert_eq!(show_norv.params["type"], json!(2));
    let msg_norv = show_norv.params["message"].as_str().unwrap_or("");
    assert!(
        msg_norv.contains("riscv32/riscv64")
            && msg_norv.contains("settings.clang")
            && msg_norv.contains("#optional-llvm-for-clang-based-diagnostics-and-expansions")
            && !msg_norv.contains("LLVM 19"),
        "got: {msg_norv}"
    );
    // Second edit must not re-send the showMessage warning
    client_cfg_norv.change_doc_incremental(
        &uri,
        2,
        lsp_types::Range::new(
            lsp_types::Position::new(0, 4),
            lsp_types::Position::new(0, 8),
        ),
        "mv  ",
    );
    assert!(
        client_cfg_norv
            .wait_for_notification("window/showMessage", std::time::Duration::from_millis(80))
            .is_none(),
        "expected at most one showMessage per configured tool"
    );
    let hv_cfg = hover_markdown(&mut client_cfg_norv, &uri, 0, 5).expect("hover keeps working");
    assert!(hv_cfg.contains("mv rd, rs"), "got: {hv_cfg}");

    client_cfg_norv.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "requires clang"]
fn march_implications_match_clang_print_enabled_extensions() {
    assert!(
        has_tool("clang"),
        "clang must be on PATH for ignored tool tests"
    );

    use riscv_asm_lsp::context::MarchSpec;
    use std::collections::BTreeSet;
    use std::process::Command;

    let cases = [
        ("riscv64-unknown-elf", "rv64gc"),
        ("riscv32-unknown-elf", "rv32gc"),
        ("riscv64-unknown-elf", "rv64imac"),
        ("riscv64-unknown-elf", "rv64gcv"),
        ("riscv64-unknown-elf", "rv64gb"),
        ("riscv64-unknown-elf", "rv64g_zk"),
        ("riscv64-unknown-elf", "rv64g_zks"),
        ("riscv64-unknown-elf", "rv64i_zca"),
        ("riscv64-unknown-elf", "rv64g_zca"),
        ("riscv64-unknown-elf", "rv64g_zcd"),
        ("riscv32-unknown-elf", "rv32imafc"),
        ("riscv32-unknown-elf", "rv32g_zca_zcd"),
        ("riscv32-unknown-elf", "rv32g_zcf_zcd"),
        ("riscv64-unknown-elf", "rv64g_zfh"),
        ("riscv64-unknown-elf", "rv64i_zaamo_zalrsc"),
        ("riscv64-unknown-elf", "rv64i_zba_zbb_zbs"),
    ];

    for (target, march) in cases {
        let out = Command::new("clang")
            .args([
                &format!("--target={target}"),
                &format!("-march={march}"),
                "--print-enabled-extensions",
            ])
            .output()
            .expect("run clang --print-enabled-extensions");
        assert!(
            out.status.success(),
            "clang failed for {march}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut clang_exts = BTreeSet::new();
        let mut in_table = false;
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Name ") {
                in_table = true;
                continue;
            }
            if trimmed.starts_with("ISA String:") {
                break;
            }
            if !in_table || trimmed.is_empty() || trimmed == "Experimental extensions" {
                continue;
            }
            if let Some(name) = trimmed.split_whitespace().next() {
                clang_exts.insert(name.to_ascii_lowercase());
            }
        }
        assert!(
            !clang_exts.is_empty(),
            "parsed no extensions from clang for {march}:\n{stdout}"
        );

        let mut ours = MarchSpec::parse(march).extensions;
        ours.remove("g");
        assert_eq!(
            ours, clang_exts,
            "extension set mismatch for {march}: ours={ours:?} vs clang={clang_exts:?}"
        );
    }
}

#[test]
#[ignore = "requires clang"]
fn predefined_macros_match_clang_dm() {
    assert!(
        has_tool("clang"),
        "clang must be on PATH for ignored tool tests"
    );

    use riscv_asm_lsp::context::MarchSpec;
    use riscv_asm_lsp::data::Database;
    use riscv_asm_lsp::preprocessor::predefined::predefined_macros;
    use std::collections::BTreeMap;
    use std::process::Command;

    let db = Database::load().expect("db");
    let cases: &[(&str, &str, Option<&str>, Option<&str>)] = &[
        ("riscv64-unknown-elf", "rv64gc", Some("lp64d"), None),
        ("riscv64-unknown-elf", "rv64imac", Some("lp64"), None),
        ("riscv32-unknown-elf", "rv32imac", Some("ilp32"), None),
        ("riscv32-unknown-elf", "rv32gc", Some("ilp32d"), None),
        ("riscv32-unknown-elf", "rv32imafc", Some("ilp32f"), None),
        (
            "riscv64-unknown-elf",
            "rv64gcv_zba_zbb_zbc_zbs",
            Some("lp64d"),
            None,
        ),
        ("riscv32-unknown-elf", "rv32e", Some("ilp32e"), None),
        ("riscv64-unknown-elf", "rv64e", Some("lp64e"), None),
        (
            "riscv64-unknown-elf",
            "rv64gc",
            Some("lp64d"),
            Some("medany"),
        ),
        ("riscv64-unknown-elf", "rv64i_zve32x", Some("lp64"), None),
        ("riscv64-unknown-elf", "rv64i_zmmul", Some("lp64"), None),
        ("riscv64-unknown-elf", "rv64i_zfinx", Some("lp64"), None),
    ];

    for &(target, march, abi, cmodel) in cases {
        let mut args = vec![
            format!("--target={target}"),
            format!("-march={march}"),
            "-E".to_owned(),
            "-dM".to_owned(),
            "-x".to_owned(),
            "assembler-with-cpp".to_owned(),
        ];
        if let Some(a) = abi {
            args.push(format!("-mabi={a}"));
        }
        if let Some(cm) = cmodel {
            args.push(format!("-mcmodel={cm}"));
        }
        args.push("/dev/null".to_owned());

        let out = Command::new("clang")
            .args(&args)
            .output()
            .expect("run clang -E -dM");
        assert!(
            out.status.success(),
            "clang failed for {march}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut clang_macros = BTreeMap::new();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("#define ")
                && let Some((name, val)) = rest.split_once(' ')
                && (name == "__ASSEMBLER__" || name.starts_with("__riscv"))
            {
                clang_macros.insert(name.to_owned(), val.trim().to_owned());
            }
        }

        let spec = MarchSpec::parse(march);
        let our_macros: BTreeMap<String, String> = predefined_macros(&spec, abi, cmodel, &db)
            .into_iter()
            .collect();
        assert_eq!(
            our_macros, clang_macros,
            "predefined macro mismatch for ({target}, {march}, {abi:?}, {cmodel:?})"
        );
    }
}

#[test]
fn builtin_validator_corpus_and_suppression() {
    use riscv_asm_lsp::config::Config;
    use riscv_asm_lsp::context::CompileContext;
    use riscv_asm_lsp::data::Database;
    use riscv_asm_lsp::diagnostics::run_builtin_diagnostics;

    let db = Database::load().expect("db");
    let fixtures = fixtures_dir();
    let mut ctx = CompileContext::new(Some(fixtures.clone()), Config::default());

    for name in ["kernel_entry.S", "other.S", "multibyte.S"] {
        let path = fixtures.join(name);
        let uri = path_to_uri(&path);
        let src = std::fs::read_to_string(&path).expect("read fixture");
        let file_ctx = ctx.for_uri(&uri);
        let diags = run_builtin_diagnostics(&uri, &src, &file_ctx, &db, &[]);
        assert!(
            diags.is_empty(),
            "expected zero false positives on {name}, got: {diags:?}"
        );
    }

    // Kernel-like sample corpus covering conditionals, relocations, `.option`, `.equ`, macros,
    // numeric labels, atomics, compressed, vector, and pseudos under rv64gcv.
    let mut ctx_v = CompileContext::new(
        None,
        Config {
            default_march: "rv64gcv".to_owned(),
            ..Config::default()
        },
    );
    let kernel_corpus_uri = crate::common::test_uri("corpus_kernel.S");
    let kernel_corpus_ctx = ctx_v.for_uri(&kernel_corpus_uri);
    let corpus = "\
#define FRAME_SIZE 64
#define SAVE_REG(r, idx) sd r, ((idx) * 8)(sp)
.equ CSR_SCRATCH, 0x140
.macro PUSH_TWO r1, r2
    sd \\r1, 0(sp)
    sd \\r2, 8(sp)
.endm
.section .text
.globl trap_entry
trap_entry:
#if __riscv_xlen == 64
    addi sp, sp, -FRAME_SIZE
    SAVE_REG(ra, 0)
    PUSH_TWO t0, t1
#else
    addi sp, sp, -32
#endif
.option push
.option norvc
    addi a0, zero, 1
.option pop
1:
    csrr t0, CSR_SCRATCH
    csrw sscratch, t0
    la a1, sym_table
    lui a2, %hi(sym_table)
    addi a2, a2, %lo(sym_table)
    lr.w a3, 0(a1)
    amoswap.w zero, a3, (a1)
    vsetvli t0, a0, e8, m1, ta, ma
    vadd.vv v8, v10, v12, v0.t
    fadd.s fa0, fa1, fa2, rne
    c.addi a0, 1
    bnez a0, 1b
    ret
sym_table:
    .dword 0
";
    let corpus_diags =
        run_builtin_diagnostics(&kernel_corpus_uri, corpus, &kernel_corpus_ctx, &db, &[]);
    assert_eq!(corpus_diags.len(), 1, "corpus_diags: {corpus_diags:?}");
    assert_eq!(
        corpus_diags[0].diagnostic.code,
        Some(lsp_types::NumberOrString::String(
            "inactive-branch".to_owned()
        ))
    );
    assert_eq!(
        corpus_diags[0].diagnostic.severity,
        Some(lsp_types::DiagnosticSeverity::HINT)
    );
    assert_eq!(corpus_diags[0].diagnostic.range.start.line, 15);
    assert!(
        corpus_diags[0]
            .diagnostic
            .tags
            .as_ref()
            .is_some_and(|t| t.contains(&lsp_types::DiagnosticTag::UNNECESSARY))
    );

    // Uncertain branch (unresolved #include) must NOT emit inactive-branch
    let uncertain_src = "#include \"missing_header.h\"\n#ifdef MAYBE_DEF\n    nop\n#endif\n";
    let uncertain_diags = run_builtin_diagnostics(
        &kernel_corpus_uri,
        uncertain_src,
        &kernel_corpus_ctx,
        &db,
        &[],
    );
    assert!(
        uncertain_diags.is_empty(),
        "unexpected inactive/diag on uncertain branch: {uncertain_diags:?}"
    );

    // Suppression hides inactive-branch hint too
    let suppressed_inactive = run_builtin_diagnostics(
        &kernel_corpus_uri,
        corpus,
        &kernel_corpus_ctx,
        &db,
        &["inactive-branch".to_owned()],
    );
    assert!(
        suppressed_inactive.is_empty(),
        "expected inactive-branch to be suppressed: {suppressed_inactive:?}"
    );

    // End-to-end LSP JSON-RPC test with `external: "off"` and `diagnostics.suppress`
    let mut client = LspClient::start(
        Some(json!({
            "clang": "/nonexistent/clang",
            "diagnostics": {
                "enable": true,
                "delayMs": 15,
                "external": "off",
                "suppress": ["gnu-only-directive"]
            }
        })),
        None,
    );
    let err_uri = crate::common::test_uri("builtin_lsp.S");
    client.open_doc(
        &err_uri,
        1,
        "    .func ignored_gnu\n    mvi a0, 1\n    addi a0, a1, 4096\n",
    );
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("builtin publishDiagnostics");
    let published = notif.params["diagnostics"].as_array().unwrap();
    assert_eq!(published.len(), 2, "published: {published:?}");
    assert_eq!(published[0]["code"], json!("unknown-mnemonic"));
    assert_eq!(published[1]["code"], json!("invalid-immediate"));
    client.shutdown_and_exit();
}

#[test]
#[ignore = "requires llvm-mc"]
fn builtin_validator_oracle_matches_llvm_mc() {
    assert!(
        has_tool("llvm-mc"),
        "llvm-mc must be on PATH for ignored tool tests"
    );

    use lsp_types::DiagnosticSeverity;
    use riscv_asm_lsp::config::Config;
    use riscv_asm_lsp::context::{CompileContext, MarchSpec};
    use riscv_asm_lsp::data::{Database, InstructionVariant};
    use riscv_asm_lsp::diagnostics::run_builtin_diagnostics;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let db = Database::load().expect("db");

    fn run_llvm_mc(triple: &str, mattr: &str, input: &str) -> bool {
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

    fn oracle_sample_for_class(class: &str) -> Option<&'static str> {
        match class {
            "GPR" | "GPRNoX0" | "GPRNoX2" | "GPRNoX0X2" | "GPRC" | "GPRMem" | "GPRCMem"
            | "BasePtr" | "BasePtrC" | "GPRMemZeroOffset" | "YGPR" => Some("a0"),
            "GPRX1" | "GPRX1X5" => Some("ra"),
            "GPRX5" => Some("t0"),
            "SP" | "SPMem" => Some("sp"),
            "SR07" => Some("s0"),
            "GPRPairRV32" | "GPRPairRV64" | "GPRPairCRV32" | "GPRPairNoX0RV32" | "FPR64IN32X" => {
                Some("a0")
            }
            "FPR16" | "FPR32" | "FPR32C" | "FPR64" | "FPR64C" | "FPR128" => Some("fa0"),
            "FPR16INX" | "FPR32INX" | "FPR64INX" => Some("a0"),
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
            "simm6nonzero"
            | "uimmlog2xlennonzero"
            | "c_lui_imm"
            | "simm5_plus1"
            | "imm5_zibi"
            | "uimm4_plus1"
            | "uimm5_plus1"
            | "uimm6_plus1" => Some("1"),
            "uimm8ge32" => Some("32"),
            "uimm7_eq_xlen" => Some("64"),
            "simm10_lsb0000nonzero" => Some("16"),
            "uimm10_lsb00nonzero" => Some("16"),
            "loadfpimm" => Some("0.5"),
            "rlist" | "reglist" => Some("{ra}"),
            "negstackadj" => Some("-16"),
            "stackadj" => Some("16"),
            "VMaskOp" => None,
            "VMaskCarryInOp" => Some("v0"),
            _ => Some("0"),
        }
    }

    fn synth(mnemonic: &str, variant: &InstructionVariant) -> Option<String> {
        if mnemonic == "c.nop" {
            return Some("c.nop".to_owned());
        }
        let mut line = variant.syntax.clone();
        let mut ops = variant.operands.clone();
        ops.sort_by_key(|a| std::cmp::Reverse(a.name.len()));
        for op in &ops {
            let s = oracle_sample_for_class(&op.class)?;
            let mut out = String::with_capacity(line.len());
            let mut i = 0;
            let bytes = line.as_bytes();
            while i < bytes.len() {
                if line[i..].starts_with(&op.name) {
                    let before_ok =
                        i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
                    let after_idx = i + op.name.len();
                    let after_ok = after_idx >= bytes.len()
                        || (!bytes[after_idx].is_ascii_alphanumeric() && bytes[after_idx] != b'_');
                    if before_ok && after_ok {
                        out.push_str(s);
                        i = after_idx;
                        continue;
                    }
                }
                if let Some(ch) = line[i..].chars().next() {
                    out.push(ch);
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            line = out;
        }
        Some(line)
    }

    // 1. Valid instruction sweep across multiple -march configurations: zero false positives on
    // any instruction variant accepted by `llvm-mc`.
    for march_str in [
        "rv64gc",
        "rv32imac",
        "rv64gcv",
        "rv64gb",
        "rv32imaf_zca_zcf_zcmp",
    ] {
        let spec = MarchSpec::parse(march_str);
        let triple = spec.llvm_triple();
        let mattr = spec.to_llvm_mattr();

        let mut valid_doc = String::from("sym:\n");
        let mut ctx = CompileContext::new(
            None,
            Config {
                default_march: march_str.to_owned(),
                ..Config::default()
            },
        );
        let uri = crate::common::test_uri("oracle_valid.S");
        let file_ctx = ctx.for_uri(&uri);

        let mut samples = Vec::new();
        for inst in &db.raw.instructions {
            if !inst.llvm_supported || inst.mnemonic.starts_with(".insn") {
                continue;
            }
            for v in &inst.variants {
                if spec.supports_extensions(&v.required_extensions, v.xlen)
                    && let Some(line) = synth(&inst.mnemonic, v)
                {
                    samples.push(line);
                }
            }
        }
        assert!(samples.len() > 150, "too few samples for {march_str}");

        let batch: String = std::iter::once("sym:\n".to_owned())
            .chain(samples.iter().map(|l| format!("    {l}\n")))
            .collect();
        let accepted = if run_llvm_mc(triple, &mattr, &batch) {
            samples
        } else {
            samples
                .into_iter()
                .filter(|l| run_llvm_mc(triple, &mattr, &format!("sym:\n    {l}\n")))
                .collect()
        };
        assert!(!accepted.is_empty());
        for l in &accepted {
            valid_doc.push_str("    ");
            valid_doc.push_str(l);
            valid_doc.push('\n');
        }
        let diags = run_builtin_diagnostics(&uri, &valid_doc, &file_ctx, &db, &[]);
        assert!(
            diags.is_empty(),
            "false positive(s) under {march_str}: {diags:#?}"
        );
    }

    // 2. Mutated invalid samples: `llvm-mc` rejects each and built-in validator reports expected error code.
    let invalid_cases: &[(&str, &str, &str)] = &[
        ("rv64gc", "    c.lwsp x0, 0(sp)\n", "invalid-register"),
        ("rv64gc", "    c.addi4spn t0, sp, 4\n", "invalid-register"),
        ("rv64gc", "    fadd.s a0, f1, f2\n", "invalid-register"),
        ("rv64gc", "    c.lui sp, 1\n", "invalid-register"),
        ("rv64gc", "    c.jr x0\n", "invalid-register"),
        ("rv64gc", "    addi a0, a1, 4096\n", "invalid-immediate"),
        ("rv64gc", "    addi a0, a1, -2049\n", "invalid-immediate"),
        ("rv64gc", "    slli a0, a1, 64\n", "invalid-immediate"),
        ("rv32imac", "    slli a0, a1, 32\n", "invalid-immediate"),
        (
            "rv64gc",
            "    csrrwi a0, mstatus, 32\n",
            "invalid-immediate",
        ),
        ("rv64gc", "    beq a0, a1, 3\n", "invalid-immediate"),
        ("rv64gc", "    c.lwsp a0, 2(sp)\n", "invalid-immediate"),
        ("rv64gc", "    lr.w a0, 4(a1)\n", "invalid-immediate"),
        ("rv64gc", "    sh1add a0, a1, a2\n", "extension-disabled"),
        ("rv64gc", "    vadd.vv v1, v2, v3\n", "extension-disabled"),
        (
            "rv32imac",
            "    fadd.s fa0, fa1, fa2\n",
            "extension-disabled",
        ),
        ("rv32imac", "    addiw a0, a1, 1\n", "xlen-mismatch"),
        ("rv32imac", "    ld a0, 0(sp)\n", "extension-disabled"),
        ("rv64gc", "    csrr a0, mcycleh\n", "invalid-csr"),
        ("rv64gc", "    csrr a0, mrmcfg\n", "invalid-csr"),
        ("rv64gc", "    csrr a0, not_a_csr\n", "invalid-csr"),
        ("rv64gc", "    addi a0, 1\n", "operand-count"),
        ("rv64gc", "    add a0, a1\n", "operand-count"),
        ("rv64gc", "    mvi a0, 1\n", "unknown-mnemonic"),
    ];

    for &(march_str, line, expected_code) in invalid_cases {
        let spec = MarchSpec::parse(march_str);
        assert!(
            !run_llvm_mc(spec.llvm_triple(), &spec.to_llvm_mattr(), line),
            "expected llvm-mc to reject `{}` under {march_str}",
            line.trim()
        );
        let mut ctx = CompileContext::new(
            None,
            Config {
                default_march: march_str.to_owned(),
                ..Config::default()
            },
        );
        let uri = crate::common::test_uri("oracle_invalid.S");
        let file_ctx = ctx.for_uri(&uri);
        let diags = run_builtin_diagnostics(&uri, line, &file_ctx, &db, &[]);
        assert!(
            diags.iter().any(|d| {
                d.diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                    && d.diagnostic.code
                        == Some(lsp_types::NumberOrString::String(expected_code.to_owned()))
            }),
            "expected code `{expected_code}` for `{}` under {march_str}, got: {diags:?}",
            line.trim()
        );
    }
}

#[cfg(unix)]
#[test]
fn default_diagnostics_uses_builtin_without_spawning_clang() {
    use std::os::unix::fs::PermissionsExt;

    let tmp =
        std::env::temp_dir().join(format!("riscv_asm_lsp_default_off_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let bin_dir = tmp.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let marker = tmp.join("clang_was_invoked");
    let fake_clang = bin_dir.join("clang");
    std::fs::write(
        &fake_clang,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.to_string_lossy()),
    )
    .unwrap();
    std::fs::set_permissions(&fake_clang, std::fs::Permissions::from_mode(0o755)).unwrap();

    let doc_uri = crate::common::test_uri("default_off.S");
    let mut client = LspClient::start_with_env(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 15 } })),
        None,
        &[("PATH", bin_dir.to_str().unwrap())],
    );
    client.open_doc(&doc_uri, 1, "    mvi a0, 1\n");
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("default built-in publishDiagnostics");
    let diags = notif.params["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "diags: {diags:?}");
    assert_eq!(diags[0]["code"], json!("unknown-mnemonic"));
    assert!(
        !marker.exists(),
        "external clang must not run when diagnostics.external defaults to off"
    );

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn did_change_configuration_preserves_initialization_options_base_layer() {
    let doc_uri = crate::common::test_uri("f1_config_layer.S");
    let mut client = LspClient::start(
        Some(json!({
            "defaultTarget": "riscv32-unknown-elf",
            "defaultMarch": "rv32imac",
            "diagnostics": { "enable": true, "delayMs": 15 }
        })),
        None,
    );
    client.open_doc(&doc_uri, 1, "_start:\n    addiw a0, a1, 1\n");
    let first = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("initial diagnostics");
    assert_eq!(
        first.params["diagnostics"][0]["code"],
        json!("xlen-mismatch")
    );

    for payload in [
        json!(null),
        json!({}),
        json!({ "riscv-asm-lsp": { "diagnostics": { "external": "off" } } }),
    ] {
        client.send_notification(
            "workspace/didChangeConfiguration",
            json!({ "settings": payload }),
        );
        let notif = client
            .wait_for_notification(
                "textDocument/publishDiagnostics",
                std::time::Duration::from_secs(3),
            )
            .expect("diagnostics after partial/null didChangeConfiguration");
        assert_eq!(
            notif.params["diagnostics"][0]["code"],
            json!("xlen-mismatch"),
            "partial didChangeConfiguration wiped initOptions: {notif:?}"
        );
    }

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "defaultMarch": "rv64gc" } }),
    );
    let cleared = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("diagnostics after switching defaultMarch to rv64gc");
    assert_eq!(cleared.params["diagnostics"], json!([]));

    client.shutdown_and_exit();
}

#[test]
fn insn_and_attribute_arch_directives_produce_no_false_positives() {
    let doc_uri = crate::common::test_uri("insn_and_attr.S");
    let mut client = LspClient::start(
        Some(json!({
            "defaultMarch": "rv64imac",
            "diagnostics": { "enable": true, "delayMs": 15 }
        })),
        None,
    );
    let src = "\
_start:
    .insn r 0x33, 0, 0, a0, a1, a2
    .insn i 0x13, 0, a0, a1, 1
    .insn 0x13
    .insn 4, 0x13
    .attribute arch, \"rv64gcv\"
    vsetivli a0, 4, e32, m1, ta, ma
    frcsr a1
";
    client.open_doc(&doc_uri, 1, src);
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("publishDiagnostics");
    assert_eq!(notif.params["diagnostics"], json!([]));
    client.shutdown_and_exit();
}

#[test]
fn deeply_nested_expressions_do_not_overflow_stack() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_deep_expr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let terms_expr = std::iter::repeat_n("1", 20_000)
        .collect::<Vec<_>>()
        .join("+");
    let parens_expr = format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000));
    let unary_expr = format!("{}1", "-".repeat(10_000));

    let unopened_path = tmp.join("deep_unopened.S");
    let deep_src = format!(
        "_start:\n    addi a0, a0, {terms_expr}\n    addi a1, a1, {parens_expr}\n    addi a2, a2, {unary_expr}\n"
    );
    std::fs::write(&unopened_path, &deep_src).unwrap();

    let open_path = tmp.join("main.S");
    let open_src = "_start:\n    addi a0, a0, 1\n";
    std::fs::write(&open_path, open_src).unwrap();

    let root_uri = path_to_uri(&tmp);
    let open_uri = path_to_uri(&open_path);
    let mut client = LspClient::start(
        Some(json!({ "diagnostics": { "enable": true, "delayMs": 15 } })),
        Some(&root_uri),
    );
    client.open_doc(&open_uri, 1, &deep_src);

    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(10),
        )
        .expect("diagnostics for deep expression file");
    assert_eq!(notif.params["uri"], json!(open_uri.as_str()));

    let hover = crate::common::hover_markdown(&mut client, &open_uri, 1, 5).expect("hover addi");
    assert!(hover.contains("addi"), "unexpected hover: {hover}");

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn validator_catches_mem_offset_missing_mem_unknown_directive_and_uses_readable_wording() {
    let doc_uri = crate::common::test_uri("f5_f6_coverage.S");
    let mut client = LspClient::start(
        Some(json!({
            "defaultMarch": "rv64imac",
            "diagnostics": { "enable": true, "delayMs": 15 }
        })),
        None,
    );
    let src = "\
_start:
    lw a0, 2048(sp)
    sd a0, a1
    .nosuchdirective 123
    c.lw a0, 0(t0)
    vsetivli a0, 4, e32, m1, ta, ma
    c.fld fs0, 0(a1)
";
    client.open_doc(&doc_uri, 1, src);
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("publishDiagnostics");
    let diags = notif.params["diagnostics"].as_array().unwrap();
    let by_line = |line: u64| -> &serde_json::Value {
        diags
            .iter()
            .find(|d| d["range"]["start"]["line"] == line)
            .unwrap_or_else(|| panic!("missing diagnostic on line {line}: {diags:?}"))
    };

    let lw_diag = by_line(1);
    assert_eq!(lw_diag["code"], json!("invalid-immediate"));
    let lw_msg = lw_diag["message"].as_str().unwrap();
    assert!(
        lw_msg.contains("integer in [-2048, 2047]") && !lw_msg.contains("simm12_lo"),
        "unexpected lw message: {lw_msg}"
    );

    let sd_diag = by_line(2);
    assert_eq!(sd_diag["code"], json!("invalid-register"));
    let sd_msg = sd_diag["message"].as_str().unwrap();
    assert!(
        !sd_msg.contains("GPRMem"),
        "unexpected sd message: {sd_msg}"
    );

    let dir_diag = by_line(3);
    assert_eq!(dir_diag["code"], json!("unknown-directive"));

    let clw_diag = by_line(4);
    assert_eq!(clw_diag["code"], json!("invalid-register"));
    let clw_msg = clw_diag["message"].as_str().unwrap();
    assert!(
        clw_msg.contains("x8–x15") && !clw_msg.contains("BasePtrC"),
        "unexpected c.lw message: {clw_msg}"
    );

    let v_diag = by_line(5);
    assert_eq!(v_diag["code"], json!("extension-disabled"));
    let v_msg = v_diag["message"].as_str().unwrap();
    assert!(
        !v_msg.contains('|'),
        "extension notice should use one separator: {v_msg}"
    );

    let flw_diag = by_line(6);
    let flw_msg = flw_diag["message"].as_str().unwrap();
    let flw_parts: Vec<&str> = flw_msg.split(" / ").collect();
    let unique: std::collections::HashSet<&str> = flw_parts.iter().copied().collect();
    assert_eq!(
        flw_parts.len(),
        unique.len(),
        "duplicate extensions in message: {flw_msg}"
    );

    client.shutdown_and_exit();
}

#[test]
fn grammar_edge_cases_and_alternative_macro_produce_no_false_diagnostics() {
    let doc_uri = crate::common::test_uri("d4_d5_grammar.S");
    let mut client = LspClient::start(
        Some(json!({
            "defaultMarch": "rv64imac",
            "diagnostics": { "enable": true, "delayMs": 15 }
        })),
        None,
    );
    let src = "\
#define SET_FAULT_HANDLER(reg, label)
#define ALTERNATIVE(old, new, feat) old
SYM_VAL = 4 + 2
.text
_start:
    SET_FAULT_HANDLER(x0, a1)
    SET_FAULT_HANDLER( \\
        x0, \\
        a1)
    .option arch, +a, +c
    amoswap.w a0, a1, (a2)
    .dc.a _start
    ALTERNATIVE(nop, \"1: nop\\n\\tj 1b\", 0)
    ALTERNATIVE(nop, __stringify(inline_alt: addi a0, a0, 1; j inline_alt), 0)
";
    client.open_doc(&doc_uri, 1, src);
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("publishDiagnostics");
    assert_eq!(notif.params["diagnostics"], json!([]));
    client.shutdown_and_exit();
}

#[test]
fn misalignment_wording_and_dtpreldword_and_sdata_directive_checks() {
    let doc_uri = crate::common::test_uri("n3_n4_checks.S");
    let mut client = LspClient::start(
        Some(json!({
            "defaultMarch": "rv64imac",
            "diagnostics": { "enable": true, "delayMs": 15 }
        })),
        None,
    );
    let src = "\
_start:
    c.lw a0, 2(a1)
    .dtpreldword 0
    .sdata
    .tdata
    .tbss
    .rodata
";
    client.open_doc(&doc_uri, 1, src);
    let notif = client
        .wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(3),
        )
        .expect("publishDiagnostics");
    let diags = notif.params["diagnostics"].as_array().unwrap();
    let by_line = |line: u64| -> &serde_json::Value {
        diags
            .iter()
            .find(|d| d["range"]["start"]["line"] == line)
            .unwrap_or_else(|| panic!("missing diagnostic on line {line}: {diags:?}"))
    };

    let clw_diag = by_line(1);
    assert_eq!(clw_diag["code"], json!("invalid-immediate"));
    let clw_msg = clw_diag["message"].as_str().unwrap();
    assert!(
        clw_msg.contains("immediate must be a multiple of 4 in [0, 124]"),
        "unexpected misaligned immediate message: {clw_msg}"
    );
    assert_eq!(clw_msg.matches("multiple of 4").count(), 1);

    let dtp_diag = by_line(2);
    assert_eq!(dtp_diag["code"], json!("gnu-only-directive"));
    assert_eq!(dtp_diag["severity"], json!(2));

    let sdata_diag = by_line(3);
    assert_eq!(sdata_diag["code"], json!("unknown-directive"));
    assert_eq!(sdata_diag["severity"], json!(1));

    for valid_line in [4u64, 5, 6] {
        assert!(
            !diags
                .iter()
                .any(|d| d["range"]["start"]["line"] == valid_line),
            "unexpected diagnostic on valid section directive line {valid_line}: {diags:?}"
        );
    }

    client.shutdown_and_exit();
}
