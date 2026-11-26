use crate::common::{LspClient, fixtures_dir, path_to_uri};
use serde_json::json;

fn legend_token_types(client: &LspClient) -> Vec<String> {
    let legend = client
        .init_result
        .capabilities
        .semantic_tokens_provider
        .as_ref()
        .expect("semantic tokens capability");
    match legend {
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
    }
}

fn decode_semantic_tokens(src: &str, raw: &[u32]) -> Vec<(String, u32, u32)> {
    let lines: Vec<&str> = src.lines().collect();
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

    let token_types = legend_token_types(&client);
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
    let decoded = decode_semantic_tokens(&kernel_src, &full_data);

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
    let range_decoded = decode_semantic_tokens(&kernel_src, &range_data);
    assert!(range_decoded.contains(&("mhartid".to_owned(), type_idx("csr"), 0)));
    assert!(range_decoded.contains(&("stvec".to_owned(), type_idx("csr"), 0)));
    assert!(!range_decoded.contains(&("KERNEL_BASE".to_owned(), type_idx("constant"), 1)));
    assert!(!range_decoded.contains(&("trap_vector".to_owned(), type_idx("label"), 1)));

    // Item 4d + Item 7: `#define` inside `.S` and `.irp` parameter declaration/usage
    let local_uri = path_to_uri(&fixtures.join("semantic_define_virtual.S"));
    let local_src = "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n.irp reg_arg, a0, a1\n    mv \\reg_arg, zero\n.endr\n";
    client.open_doc(&local_uri, 1, local_src);
    let local_resp = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": local_uri.as_str() } }),
    );
    let local_data = local_resp.result.expect("semanticTokens/full")["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect::<Vec<_>>();
    let local_decoded = decode_semantic_tokens(local_src, &local_data);
    assert!(local_decoded.contains(&("LOCAL_VALUE".to_owned(), type_idx("constant"), 1)));
    assert!(local_decoded.contains(&("LOCAL_VALUE".to_owned(), type_idx("constant"), 0)));
    assert!(local_decoded.contains(&("reg_arg".to_owned(), type_idx("parameter"), 1)));
    assert!(local_decoded.contains(&("\\reg_arg".to_owned(), type_idx("parameter"), 0)));

    client.shutdown_and_exit();
}

#[test]
fn semantic_tokens_distinguish_preprocessor_and_comment_per_file() {
    let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_sem_pp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let pre_s = tmp.join("pre.S");
    let raw_s = tmp.join("raw.s");
    let forced_pre = tmp.join("forced.s");
    let forced_raw = tmp.join("forced_raw.S");

    let src = "\
#include \"regs.h\"
# define X 5
# 100 note
# just a comment
_start:
    li a0, X
    nop # if define error
";
    std::fs::write(&pre_s, src).unwrap();
    std::fs::write(&raw_s, src).unwrap();
    std::fs::write(&forced_pre, src).unwrap();
    std::fs::write(&forced_raw, src).unwrap();
    std::fs::write(
        tmp.join("compile_commands.json"),
        json!([
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "-x", "assembler-with-cpp", "-c", forced_pre.to_string_lossy()],
                "file": forced_pre.to_string_lossy()
            },
            {
                "directory": tmp.to_string_lossy(),
                "arguments": ["clang", "-x", "assembler", "-c", forced_raw.to_string_lossy()],
                "file": forced_raw.to_string_lossy()
            }
        ])
        .to_string(),
    )
    .unwrap();

    let root_uri = path_to_uri(&tmp);
    let pre_uri = path_to_uri(&pre_s);
    let raw_uri = path_to_uri(&raw_s);
    let forced_pre_uri = path_to_uri(&forced_pre);
    let forced_raw_uri = path_to_uri(&forced_raw);

    let mut client = LspClient::start(None, Some(&root_uri));
    let token_types = legend_token_types(&client);
    let idx_of = |name: &str| {
        token_types
            .iter()
            .position(|t| t == name)
            .expect("legend type") as u32
    };
    let pp_idx = idx_of("preprocessor");
    let comment_idx = idx_of("comment");
    let const_idx = idx_of("constant");

    for uri in [&pre_uri, &forced_pre_uri] {
        client.open_doc(uri, 1, src);
        let resp = client.send_request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        let data: Vec<u32> = resp.result.expect("tokens")["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        let decoded = decode_semantic_tokens(src, &data);
        assert!(decoded.contains(&("#include".to_owned(), pp_idx, 0)));
        assert!(decoded.contains(&("# define".to_owned(), pp_idx, 0)));
        assert!(decoded.contains(&("# 100".to_owned(), pp_idx, 0)));
        assert!(decoded.contains(&("X".to_owned(), const_idx, 1)));
        assert!(decoded.contains(&("X".to_owned(), const_idx, 0)));
        assert!(
            !decoded
                .iter()
                .any(|(lex, _, _)| lex == "# just a comment" || lex.contains("if define error"))
        );
    }

    for uri in [&raw_uri, &forced_raw_uri] {
        client.open_doc(uri, 1, src);
        let resp = client.send_request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        let data: Vec<u32> = resp.result.expect("tokens")["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        let decoded = decode_semantic_tokens(src, &data);
        assert!(decoded.contains(&("#include \"regs.h\"".to_owned(), comment_idx, 0)));
        assert!(decoded.contains(&("# define X 5".to_owned(), comment_idx, 0)));
        assert!(decoded.contains(&("# 100 note".to_owned(), comment_idx, 0)));
        assert!(decoded.contains(&("# just a comment".to_owned(), comment_idx, 0)));
        assert!(!decoded.iter().any(|(_, ty, _)| *ty == pp_idx));
        assert!(
            !decoded
                .iter()
                .any(|(lex, ty, _)| lex == "X" && *ty == const_idx)
        );
    }

    client.shutdown_and_exit();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn semantic_tokens_prefer_user_symbols_over_non_csr_positions_and_skip_block_comments() {
    let mut client = LspClient::start(None, None);
    let uri = crate::common::test_uri("sem_csr_and_blk.S");
    let src = "\
/*
#define GHOST_CONST 1
*/
time:
    ret
_start:
    call time
    csrr a0, time
";
    client.open_doc(&uri, 1, src);

    let token_types = legend_token_types(&client);
    let idx_of = |name: &str| {
        token_types
            .iter()
            .position(|t| t == name)
            .expect("legend type") as u32
    };
    let label_idx = idx_of("label");
    let csr_idx = idx_of("csr");
    let const_idx = idx_of("constant");
    let pp_idx = idx_of("preprocessor");

    let resp = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );
    let data: Vec<u32> = resp.result.expect("tokens")["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    let decoded = decode_semantic_tokens(src, &data);

    // `#define GHOST_CONST 1` inside `/* ... */` produces neither preprocessor nor constant tokens
    assert!(
        !decoded
            .iter()
            .any(|(lex, ty, _)| lex == "#define" && *ty == pp_idx)
    );
    assert!(
        !decoded
            .iter()
            .any(|(lex, ty, _)| lex == "GHOST_CONST" && *ty == const_idx)
    );

    // `call time` uses `label`, while `csrr a0, time` still uses `csr`
    let time_tokens: Vec<(u32, u32)> = decoded
        .iter()
        .filter(|(lex, _, _)| lex == "time")
        .map(|(_, ty, m)| (*ty, *m))
        .collect();
    assert_eq!(
        time_tokens,
        vec![(label_idx, 1), (label_idx, 0), (csr_idx, 0)],
        "decoded: {decoded:?}"
    );

    // Inactive `#if 0` / `#else` greying emits comment tokens for inactive body while keeping active code & `#if`/`#else`/`#endif`
    let inact_uri = crate::common::test_uri("sem_inactive.S");
    let inact_src = "\
#if 0
    addi a0, a1, 1
#else
    nop
#endif
#include \"missing_guard.h\"
#ifdef MAYBE
    addi a0, a1, 2
#endif
";
    client.open_doc(&inact_uri, 1, inact_src);
    let inact_resp = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": inact_uri.as_str() } }),
    );
    let inact_data: Vec<u32> = inact_resp.result.expect("inactive tokens")["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    let inact_decoded = decode_semantic_tokens(inact_src, &inact_data);
    let comment_idx = idx_of("comment");
    let reg_idx = idx_of("register");
    assert!(
        inact_decoded.contains(&("#if".to_owned(), pp_idx, 0)),
        "inact_decoded: {inact_decoded:?}"
    );
    assert!(
        inact_decoded.contains(&("    addi a0, a1, 1".to_owned(), comment_idx, 0)),
        "inact_decoded: {inact_decoded:?}"
    );
    assert!(inact_decoded.contains(&("#else".to_owned(), pp_idx, 0)));
    assert!(inact_decoded.contains(&("#endif".to_owned(), pp_idx, 0)));
    // Uncertain branch stays normally highlighted (`a0`, `a1` are registers, not greyed out)
    assert!(
        !inact_decoded.contains(&("    addi a0, a1, 2".to_owned(), comment_idx, 0)),
        "uncertain branch should not be greyed out: {inact_decoded:?}"
    );
    assert!(inact_decoded.contains(&("a0".to_owned(), reg_idx, 0)));

    client.shutdown_and_exit();
}
