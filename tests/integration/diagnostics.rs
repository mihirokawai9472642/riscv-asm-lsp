use crate::common::{LspClient, fixtures_dir, has_tool, hover_markdown, path_to_uri};
use serde_json::json;

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

    let md = hover_markdown(&mut client, &uri, 0, 5).expect("hover");
    assert!(md.contains("mv"));
    assert!(!md.contains("**This line:**"));

    std::thread::sleep(std::time::Duration::from_millis(80));
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
            "diagnostics": { "enable": true, "delayMs": 30 }
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
