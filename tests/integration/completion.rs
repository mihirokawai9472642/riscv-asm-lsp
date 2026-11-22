use crate::common::{
    LspClient, completion_items_vec, completion_labels, fixtures_dir, path_to_uri,
};
use serde_json::json;

#[test]
fn completion_respects_march_and_operand_position() {
    let mut client = LspClient::start(Some(json!({ "defaultMarch": "rv32i" })), None);
    let uri: lsp_types::Uri = "file:///tmp/comp_test.S".parse().expect("uri");
    client.open_doc(&uri, 1, "    \n    csrw \n");

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
    assert!(fadd_s["sortText"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(!fadd_s["documentation"].is_null());
    assert!(fadd_s["data"].is_null());

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
            .all(|i| i["label"].as_str().is_some_and(|s| s.starts_with('%')))
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
    assert!(add_sort < c_add_sort);
    assert!(add_sort < align_sort);

    client.shutdown_and_exit();
}

#[test]
fn completion_snippets_variant_order_distinct_labels_and_separators() {
    // 1. Snippet-capable client (Items 2, 4c, E2)
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

    // Item 2: `add` variants order: real -> alias -> special TLS form
    let mut add_items: Vec<_> = stmt_items
        .iter()
        .filter(|i| i["filterText"] == "add")
        .collect();
    add_items.sort_by(|a, b| a["sortText"].as_str().cmp(&b["sortText"].as_str()));
    let add_labels: Vec<&str> = add_items
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert_eq!(
        add_labels,
        vec![
            "add rd, rs1, rs2",
            "add rd, rs1, imm12",
            "add rd, rs1, rs2, src",
        ]
    );

    let addi = stmt_items
        .iter()
        .find(|i| i["filterText"] == "addi")
        .expect("addi snippet");
    assert_eq!(addi["label"], json!("addi rd, rs1, imm12"));
    assert_eq!(addi["detail"], json!("Add immediate"));
    assert_eq!(addi["insertTextFormat"], json!(2));
    assert_eq!(
        addi["textEdit"]["newText"],
        json!("addi ${1:rd}, ${2:rs1}, ${3:imm12}$0")
    );

    // Item 4c: `ld` variants have distinct labels & docs starting with variant syntax
    let mut ld_items: Vec<_> = stmt_items
        .iter()
        .filter(|i| i["filterText"] == "ld")
        .collect();
    ld_items.sort_by(|a, b| a["sortText"].as_str().cmp(&b["sortText"].as_str()));
    assert!(ld_items.len() >= 2);
    assert_eq!(ld_items[0]["label"], json!("ld rd, imm12(rs1)"));
    assert_eq!(ld_items[0]["detail"], json!("Load doubleword"));
    assert_eq!(
        ld_items[0]["textEdit"]["newText"],
        json!("ld ${1:rd}, ${2:0}(${3:rs1})$0")
    );
    assert!(ld_items[0]["documentation"].is_null());

    let resolved_ld = snip_client
        .send_request("completionItem/resolve", ld_items[0].clone())
        .result
        .expect("resolved ld");
    assert!(
        resolved_ld["documentation"]["value"]
            .as_str()
            .unwrap_or("")
            .starts_with("```riscv\nld rd, imm12(rs1)\n```")
    );

    // Item 5: deprecated aliases carry DEPRECATED tag (1) and sort after preferred names
    let frsr = stmt_items
        .iter()
        .find(|i| i["filterText"] == "frsr")
        .expect("frsr");
    assert_eq!(frsr["tags"], json!([1]));
    let frcsr = stmt_items
        .iter()
        .find(|i| i["filterText"] == "frcsr")
        .expect("frcsr");
    assert!(frcsr["sortText"].as_str() < frsr["sortText"].as_str());
    let resolved_frsr = snip_client
        .send_request("completionItem/resolve", frsr.clone())
        .result
        .expect("resolved frsr");
    assert!(
        resolved_frsr["documentation"]["value"]
            .as_str()
            .unwrap_or("")
            .contains("Deprecated — prefer `frcsr`")
    );

    let mut csrw_items: Vec<_> = stmt_items
        .iter()
        .filter(|i| i["filterText"] == "csrw")
        .collect();
    csrw_items.sort_by(|a, b| a["sortText"].as_str().cmp(&b["sortText"].as_str()));
    assert_eq!(csrw_items.len(), 2);
    assert_eq!(csrw_items[0]["label"], json!("csrw csr, rs"));
    assert_eq!(csrw_items[1]["label"], json!("csrw csr, imm"));
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
        .find(|i| i["filterText"] == ".macro")
        .expect(".macro snippet");
    assert_eq!(mac["label"], json!(".macro name args"));
    assert_eq!(
        mac["textEdit"]["newText"],
        json!(".macro ${1:name} ${2:args}\n\t$0\n.endm")
    );

    let mut opt_items: Vec<_> = dir_items
        .iter()
        .filter(|i| i["filterText"] == ".option")
        .collect();
    opt_items.sort_by(|a, b| a["sortText"].as_str().cmp(&b["sortText"].as_str()));
    assert_eq!(opt_items.len(), 2);
    assert_eq!(opt_items[0]["label"], json!(".option push ... pop"));
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
fn completion_caps_large_symbol_lists_with_is_incomplete_and_prefix_filter() {
    use std::fmt::Write;
    let mut client = LspClient::start(None, None);
    let uri = path_to_uri(&fixtures_dir().join("many_syms_virtual.S"));
    let mut src = String::new();
    for i in 0..260 {
        let _ = writeln!(src, "label_{i:03}:\n    nop");
    }
    src.push_str("    j \n    j label_25\n");
    client.open_doc(&uri, 1, &src);

    let unfiltered = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 520, "character": 6 }
        }),
    );
    let res = unfiltered.result.as_ref().expect("CompletionList");
    assert_eq!(res["isIncomplete"], json!(true));
    let sym_items = res["items"]
        .as_array()
        .expect("items array")
        .iter()
        .filter(|i| i["kind"] == json!(18)) // CompletionItemKind::REFERENCE == 18
        .count();
    assert_eq!(sym_items, 200);

    let filtered = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 521, "character": 14 }
        }),
    );
    let labels = completion_labels(&filtered.result);
    assert!(labels.iter().any(|l| l == "label_259"));

    client.shutdown_and_exit();
}
