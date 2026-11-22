use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use riscv_asm_lsp::data::DirectiveEntry;

#[derive(Debug, Clone)]
pub struct ManualPseudo {
    pub mnemonic: String,
    pub syntax: String,
    pub expansion: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct AsmManualData {
    pub pseudos: Vec<ManualPseudo>,
    pub directives: Vec<DirectiveEntry>,
}

/// Directives documented in riscv-asm-manual that LLVM 19 rejects (Design Appendix A).
const LLVM19_UNSUPPORTED_DIRECTIVES: &[&str] = &[
    ".dtprelword",
    ".eqv",
    ".func",
    ".endfunc",
    ".float16",
    ".bfloat16",
    ".base64",
    ".linkonce",
    ".stabs",
    ".list",
    ".nolist",
    ".title",
    ".sbttl",
    ".psize",
    ".mri",
];

pub fn load(asm_manual_dir: &Path) -> Result<AsmManualData, Box<dyn std::error::Error>> {
    let adoc_path = asm_manual_dir.join("src/asm-manual.adoc");
    let text = fs::read_to_string(&adoc_path)?;

    let directives = parse_directives(&text);
    let pseudos = parse_pseudos(&text);

    Ok(AsmManualData {
        pseudos,
        directives,
    })
}

fn parse_directives(text: &str) -> Vec<DirectiveEntry> {
    let rows = extract_table_rows(text, ".Assembler Directives");
    let mut out = Vec::new();
    for (idx, row) in rows.into_iter().enumerate() {
        if idx == 0 || row.len() < 3 {
            continue;
        }
        let raw_name = clean_cell(&row[0]);
        if !raw_name.starts_with('.') {
            continue;
        }
        let name = raw_name.to_ascii_lowercase();
        let args = clean_cell(&row[1]);
        let desc = clean_cell(&row[2]);
        let llvm_supported = !LLVM19_UNSUPPORTED_DIRECTIVES.contains(&name.as_str());

        out.push(DirectiveEntry {
            name,
            arguments: if args.is_empty() { None } else { Some(args) },
            description: desc,
            source: "riscv-asm-manual (CC-BY-4.0)".to_owned(),
            llvm_supported,
        });
    }
    out
}

fn parse_pseudos(text: &str) -> Vec<ManualPseudo> {
    let mut by_mnemonic: BTreeMap<String, ManualPseudo> = BTreeMap::new();
    for title in [
        ".Pseudo Instructions",
        ".Pseudoinstructions for acccessing control and status registers",
    ] {
        let rows = extract_table_rows(text, title);
        for (idx, row) in rows.into_iter().enumerate() {
            if idx == 0 || row.len() < 3 {
                continue;
            }
            let raw_syntax = clean_cell(&row[0]);
            let expansion = clean_cell(&row[1]);
            let meaning = clean_cell(&row[2]);
            let comment = row.get(3).map(|s| clean_cell(s)).unwrap_or_default();

            let description = if comment.is_empty() {
                meaning
            } else {
                format!("{meaning} — {comment}")
            };

            let first_token = raw_syntax.split_whitespace().next().unwrap_or("");
            for mnem in expand_mnemonic_pattern(first_token) {
                let lower = mnem.to_ascii_lowercase();
                let syntax = if let Some(rest) = raw_syntax.strip_prefix(first_token) {
                    format!("{lower}{rest}")
                } else {
                    raw_syntax.clone()
                };
                by_mnemonic.entry(lower.clone()).or_insert(ManualPseudo {
                    mnemonic: lower,
                    syntax,
                    expansion: expansion.clone(),
                    description: description.clone(),
                });
            }
        }
    }
    by_mnemonic.into_values().collect()
}

/// Extract AsciiDoc `|=== ... |===` table rows following `section_marker`.
/// Each row is split into cells delimited by `|`, keeping multi-line cells intact.
fn extract_table_rows(text: &str, section_marker: &str) -> Vec<Vec<String>> {
    let Some(marker_pos) = text.find(section_marker) else {
        return Vec::new();
    };
    let after_marker = &text[marker_pos..];
    let Some(open_pos) = after_marker.find("|===") else {
        return Vec::new();
    };
    let table_body_start = &after_marker[open_pos + 4..];
    let Some(close_pos) = table_body_start.find("|===") else {
        return Vec::new();
    };
    let body = &table_body_start[..close_pos];

    // Check column count from `[cols="..."]` if present, otherwise count header cells
    let header_block = &after_marker[..open_pos];
    let expected_cols = parse_cols_count(header_block).unwrap_or_else(|| {
        body.lines()
            .find(|l| l.trim_start().starts_with('|'))
            .map(|l| split_adoc_cells(l).len())
            .unwrap_or(3)
    });

    let mut rows = Vec::new();
    let mut current_row: Vec<String> = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if current_row.len() >= expected_cols {
                rows.push(std::mem::take(&mut current_row));
            }
            continue;
        }
        if trimmed.starts_with('|') {
            let cells = split_adoc_cells(trimmed);
            if cells.len() >= expected_cols {
                if !current_row.is_empty() {
                    rows.push(std::mem::take(&mut current_row));
                }
                rows.push(cells);
            } else {
                for cell in cells {
                    current_row.push(cell);
                    if current_row.len() >= expected_cols {
                        rows.push(std::mem::take(&mut current_row));
                    }
                }
            }
        } else if let Some(last) = current_row.last_mut() {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(trimmed);
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    rows
}

fn parse_cols_count(header: &str) -> Option<usize> {
    let idx = header.find("cols=")?;
    let rest = &header[idx + 5..];
    let quote_char = rest.chars().next()?;
    let inner = rest.get(1..)?.split(quote_char).next()?;
    Some(inner.split(',').count())
}

fn split_adoc_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix('|').unwrap_or(trimmed);
    // Unescape `\|` inside cells before splitting on `|`
    let sentinel = "\x00";
    let escaped = body.replace("\\|", sentinel);
    escaped
        .split('|')
        .map(|s| s.replace(sentinel, "|").trim().to_owned())
        .collect()
}

fn clean_cell(raw: &str) -> String {
    let mut s = raw
        .replace(" +", " ")
        .replace("*Myriad sequences", "Multiple instruction sequences")
        .replace("\\|", "|");
    // Strip bold markers `*text*`, footnote macros `{fn-...}`, and AsciiDoc xref `<<target, label>>`
    while let Some(start) = s.find("<<") {
        if let Some(end) = s[start + 2..].find(">>") {
            let inner = &s[start + 2..start + 2 + end];
            let replacement = inner
                .split_once(',')
                .map(|(_, l)| l.trim())
                .unwrap_or_else(|| inner.trim())
                .to_owned();
            s.replace_range(start..start + 2 + end + 2, &replacement);
        } else {
            break;
        }
    }
    while let Some(start) = s.find("{fn-") {
        if let Some(end) = s[start..].find('}') {
            s.replace_range(start..start + end + 1, "");
        } else {
            break;
        }
    }
    if s.starts_with('*') && s.ends_with('*') && s.len() >= 2 {
        s = s[1..s.len() - 1].to_owned();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn expand_mnemonic_pattern(pat: &str) -> Vec<String> {
    // Expand `rdcycle[h]` -> [`rdcycle`, `rdcycleh`]
    if let (Some(lb), Some(rb)) = (pat.find('['), pat.find(']'))
        && lb < rb
    {
        let prefix = &pat[..lb];
        let opt = &pat[lb + 1..rb];
        let suffix = &pat[rb + 1..];
        let mut out = expand_mnemonic_pattern(&format!("{prefix}{suffix}"));
        out.extend(expand_mnemonic_pattern(&format!("{prefix}{opt}{suffix}")));
        return out;
    }
    // Expand `l{b|h|w|d}` -> [`lb`, `lh`, `lw`, `ld`]
    if let (Some(lb), Some(rb)) = (pat.find('{'), pat.find('}'))
        && lb < rb
    {
        let prefix = &pat[..lb];
        let opts = &pat[lb + 1..rb];
        let suffix = &pat[rb + 1..];
        let mut out = Vec::new();
        for choice in opts.split('|') {
            out.extend(expand_mnemonic_pattern(&format!(
                "{prefix}{choice}{suffix}"
            )));
        }
        return out;
    }
    if pat.is_empty() {
        Vec::new()
    } else {
        vec![pat.to_owned()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_cell_strips_footnotes_and_leading_asterisks() {
        assert_eq!(
            clean_cell("*Myriad sequences{fn-8}"),
            "Multiple instruction sequences"
        );
        assert_eq!(clean_cell("a \\| b"), "a | b");
    }

    #[test]
    fn parse_pseudos_does_not_embed_duplicate_expands_to_clause() {
        let doc = "\
.Pseudo Instructions
|===
| Pseudoinstruction | Base Instruction(s) | Meaning | Comment
| li rd, symbol | *Myriad sequences{fn-8} | Load immediate | Arbitrary size
| mv rd, rs | addi rd, rs, 0 | Copy register |
|===
";
        let pseudos = parse_pseudos(doc);
        assert_eq!(pseudos.len(), 2);
        for p in &pseudos {
            assert!(
                !p.description.contains("expands to"),
                "description should not embed expands-to: {}",
                p.description
            );
            assert!(!p.expansion.contains("{fn-"));
        }
    }
}
