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

    // Item 4d: `#define` inside `.S` file gets `constant` (+ declaration) tokens
    let local_uri = path_to_uri(&fixtures.join("semantic_define_virtual.S"));
    let local_src = "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n";
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

    client.shutdown_and_exit();
}
