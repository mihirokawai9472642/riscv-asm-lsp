use crate::common::fixtures_dir;

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
        "overrides.scm",
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

        if file_name == "overrides.scm" {
            assert!(query.capture_index_for_name("comment.inclusive").is_some());
            assert!(query.capture_index_for_name("string").is_some());
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
            assert!(names.iter().any(|n| n.contains("_start")));
            assert!(names.iter().any(|n| n.contains("trap_vector")));
            assert!(names.iter().any(|n| n == "SAVE_REG"));
        }
    }

    let cfg_toml = std::fs::read_to_string(lang_dir.join("config.toml")).expect("read config.toml");
    assert!(cfg_toml.contains("\"riscv\""));
    assert!(cfg_toml.contains("line_comments = [\"// \", \"# \"]"));
    assert!(cfg_toml.contains("block_comment = {"));
    assert!(cfg_toml.contains("documentation_comment = {"));
    assert!(cfg_toml.contains("start = \"/*\", end = \" */\""));

    let sem_rules_src = std::fs::read_to_string(lang_dir.join("semantic_token_rules.json"))
        .expect("read semantic_token_rules.json");
    let sem_rules: serde_json::Value =
        serde_json::from_str(&sem_rules_src).expect("valid semantic_token_rules.json");
    let rules = sem_rules.as_array().expect("rules array");
    for (tok_type, fallback) in [
        ("register", "type"),
        ("csr", "enum"),
        ("macro", "preproc"),
        ("label", "label"),
        ("constant", "constant"),
        ("parameter", "variable.parameter"),
    ] {
        let entry = rules
            .iter()
            .find(|r| r["token_type"] == tok_type)
            .expect("semantic_token_rules entry");
        let styles = entry["style"].as_array().expect("style array");
        assert!(
            styles.iter().any(|s| s == fallback),
            "missing fallback {fallback} for {tok_type}: {styles:?}"
        );
    }
}
