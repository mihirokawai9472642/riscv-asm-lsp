use std::path::PathBuf;

use serde_json::Value;

pub fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("tests")
        .join("snapshots")
}

pub fn normalize_text(input: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = input.to_owned();
    for &(from, to) in replacements {
        if !from.is_empty() {
            out = out.replace(from, to);
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn assert_text_snapshot(rel_path: &str, actual: &str) {
    let mut normalized = actual.replace("\r\n", "\n");
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    let path = snapshots_dir().join(rel_path);
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create snapshot dir");
        }
        std::fs::write(&path, &normalized).expect("write snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| {
            panic!(
                "missing snapshot {}: {e} (run with UPDATE_SNAPSHOTS=1 to create)",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    if expected != normalized {
        let diff = format_line_diff(&expected, &normalized);
        panic!("snapshot mismatch for {rel_path}:\n{diff}");
    }
}

pub fn assert_json_snapshot(rel_path: &str, value: &Value) {
    let pretty = serde_json::to_string_pretty(value).expect("serialize snapshot json");
    assert_text_snapshot(rel_path, &pretty);
}

fn format_line_diff(expected: &str, actual: &str) -> String {
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    let max_len = exp_lines.len().max(act_lines.len());
    let mut out = String::new();
    let mut shown = 0usize;
    for i in 0..max_len {
        let e = exp_lines.get(i).copied();
        let a = act_lines.get(i).copied();
        if e != a {
            if shown >= 40 {
                out.push_str("... (more differing lines omitted)\n");
                break;
            }
            out.push_str(&format!("line {}:\n", i + 1));
            out.push_str(&format!("  - {}\n", e.unwrap_or("<EOF>")));
            out.push_str(&format!("  + {}\n", a.unwrap_or("<EOF>")));
            shown += 1;
        }
    }
    out
}
