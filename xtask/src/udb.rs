use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use riscv_asm_lsp::data::{CsrField, Xlen};
use yaml_serde::Value;

#[derive(Debug, Clone)]
pub struct UdbInstruction {
    pub name: String,
    pub long_name: Option<String>,
    pub description: Option<String>,
    pub assembly: Option<String>,
    pub extensions: Vec<String>,
    pub xlen: Xlen,
    pub is_pseudo: bool,
    pub alias_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UdbCsr {
    pub name: String,
    pub long_name: Option<String>,
    pub address: u16,
    pub privilege: Option<String>,
    pub extensions: Vec<String>,
    pub description: Option<String>,
    pub fields: Vec<CsrField>,
}

#[derive(Debug, Clone)]
pub struct UdbRegisterInfo {
    pub description: Option<String>,
    pub saver: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UdbData {
    pub instructions: Vec<UdbInstruction>,
    pub csrs: Vec<UdbCsr>,
    pub registers: HashMap<String, UdbRegisterInfo>,
    pub cleaned_todos: Vec<String>,
    pub copyrights: Vec<String>,
}

pub fn load(udb_dir: &Path) -> Result<UdbData, Box<dyn std::error::Error>> {
    let isa_dir = udb_dir.join("spec/std/isa");
    let mut cleaned_todos = Vec::new();
    let mut copyrights = BTreeSet::new();
    let instructions = load_instructions(&isa_dir, &mut cleaned_todos, &mut copyrights)?;
    let csrs = load_csrs(&isa_dir, &mut cleaned_todos, &mut copyrights)?;
    let registers = load_registers(&isa_dir, &mut copyrights)?;

    cleaned_todos.sort();
    cleaned_todos.dedup();
    Ok(UdbData {
        instructions,
        csrs,
        registers,
        cleaned_todos,
        copyrights: copyrights.into_iter().collect(),
    })
}

fn load_instructions(
    isa_dir: &Path,
    cleaned_todos: &mut Vec<String>,
    copyrights: &mut BTreeSet<String>,
) -> Result<Vec<UdbInstruction>, Box<dyn std::error::Error>> {
    let mut inst_files = Vec::new();
    collect_yaml_files(&isa_dir.join("inst"), &mut inst_files)?;
    inst_files.sort();

    let mut instructions = Vec::new();
    let mut pseudo_map: BTreeMap<String, UdbInstruction> = BTreeMap::new();

    for path in inst_files {
        let content = fs::read_to_string(&path)?;
        collect_license_headers(&path, &content, copyrights)?;
        let doc: Value = match yaml_serde::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if doc.get("$ref").is_some() {
            continue;
        }
        let Some(name) = doc.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mnemonic = name.trim().to_ascii_lowercase();
        let long_name = doc
            .get("long_name")
            .and_then(Value::as_str)
            .map(clean_text)
            .filter(|s| !s.is_empty());
        let description = strip_todo_sentences(
            &mnemonic,
            doc.get("description")
                .and_then(Value::as_str)
                .map(|s| clean_text_with_hpm(s, None))
                .filter(|s| !s.is_empty()),
            cleaned_todos,
        );
        let assembly = doc
            .get("assembly")
            .and_then(Value::as_str)
            .map(|s| format!("{mnemonic} {}", s.trim()).trim().to_owned());
        let extensions = extract_defined_by(doc.get("definedBy"));
        let xlen = match doc.get("base").and_then(Value::as_i64) {
            Some(32) => Xlen::Rv32,
            Some(64) => Xlen::Rv64,
            _ => Xlen::Both,
        };

        if let Some(pseudos) = doc.get("pseudoinstructions").and_then(Value::as_sequence) {
            for ps in pseudos {
                let Some(to) = ps.get("to").and_then(Value::as_str) else {
                    continue;
                };
                let cleaned_to = to.trim();
                let ps_mnem = cleaned_to
                    .split([' ', '\t'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ps_mnem.is_empty() {
                    continue;
                }
                let when = ps.get("when").and_then(Value::as_str).map(str::trim);
                let ps_desc = match when {
                    Some(w) if !w.is_empty() => {
                        format!("Pseudoinstruction for `{mnemonic}` when `{w}`.")
                    }
                    _ => format!("Pseudoinstruction for `{mnemonic}`."),
                };
                pseudo_map
                    .entry(ps_mnem.clone())
                    .or_insert_with(|| UdbInstruction {
                        name: ps_mnem,
                        long_name: None,
                        description: Some(ps_desc),
                        assembly: Some(cleaned_to.to_owned()),
                        extensions: extensions.clone(),
                        xlen,
                        is_pseudo: true,
                        alias_of: Some(mnemonic.clone()),
                    });
            }
        }

        instructions.push(UdbInstruction {
            name: mnemonic,
            long_name,
            description,
            assembly,
            extensions,
            xlen,
            is_pseudo: false,
            alias_of: None,
        });
    }

    for (_, pseudo) in pseudo_map {
        instructions.push(pseudo);
    }

    Ok(instructions)
}

fn load_csrs(
    isa_dir: &Path,
    cleaned_todos: &mut Vec<String>,
    copyrights: &mut BTreeSet<String>,
) -> Result<Vec<UdbCsr>, Box<dyn std::error::Error>> {
    let mut csr_files = Vec::new();
    collect_yaml_files(&isa_dir.join("csr"), &mut csr_files)?;
    csr_files.sort();

    let mut csrs = Vec::new();
    for path in csr_files {
        let content = fs::read_to_string(&path)?;
        collect_license_headers(&path, &content, copyrights)?;
        let doc: Value = match yaml_serde::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if doc.get("$ref").is_some() {
            continue;
        }
        let Some(name) = doc.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(address_val) = doc.get("address").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(address) = u16::try_from(address_val) else {
            continue;
        };
        let csr_name = name.trim().to_ascii_lowercase();
        let hpm_num = csr_hpm_num(&csr_name);
        let long_name = doc
            .get("long_name")
            .and_then(Value::as_str)
            .map(clean_text)
            .filter(|s| !s.is_empty());
        let privilege = doc
            .get("priv_mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let extensions = extract_defined_by(doc.get("definedBy"));
        let description = strip_todo_sentences(
            &csr_name,
            doc.get("description")
                .and_then(Value::as_str)
                .map(|s| clean_text_with_hpm(s, hpm_num.as_deref()))
                .filter(|s| !s.is_empty()),
            cleaned_todos,
        );

        let mut fields = Vec::new();
        if let Some(fields_map) = doc.get("fields").and_then(Value::as_mapping) {
            for (k, v) in fields_map {
                let Some(fname) = k.as_str() else { continue };
                if v.get("$ref").is_some() {
                    continue;
                }
                let bits = field_bits_str(v);
                let field_owner = format!("{csr_name}.{fname}");
                let fdesc = strip_todo_sentences(
                    &field_owner,
                    v.get("description")
                        .and_then(Value::as_str)
                        .map(|s| clean_text_with_hpm(s, hpm_num.as_deref()))
                        .filter(|s| !s.is_empty()),
                    cleaned_todos,
                );
                fields.push(CsrField {
                    name: fname.to_owned(),
                    bits,
                    summary: None,
                    description: fdesc,
                });
            }
        }

        csrs.push(UdbCsr {
            name: csr_name,
            long_name,
            address,
            privilege,
            extensions,
            description,
            fields,
        });
    }

    Ok(csrs)
}

fn load_registers(
    isa_dir: &Path,
    copyrights: &mut BTreeSet<String>,
) -> Result<HashMap<String, UdbRegisterInfo>, Box<dyn std::error::Error>> {
    let mut reg_files = Vec::new();
    collect_yaml_files(&isa_dir.join("register_file"), &mut reg_files)?;
    reg_files.sort();
    let mut registers = HashMap::new();
    for path in reg_files {
        let content = fs::read_to_string(&path)?;
        collect_license_headers(&path, &content, copyrights)?;
        let doc: Value = match yaml_serde::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(list) = doc.get("registers").and_then(Value::as_sequence) else {
            continue;
        };
        for item in list {
            let Some(rname) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .map(clean_text)
                .filter(|s| !s.is_empty());
            let saver = if item.get("caller_saved").and_then(Value::as_bool) == Some(true) {
                Some("caller-saved".to_owned())
            } else if item.get("callee_saved").and_then(Value::as_bool) == Some(true) {
                Some("callee-saved".to_owned())
            } else {
                None
            };
            registers.insert(
                rname.trim().to_ascii_lowercase(),
                UdbRegisterInfo { description, saver },
            );
        }
    }
    Ok(registers)
}

fn collect_license_headers(
    path: &Path,
    content: &str,
    copyrights: &mut BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut spdx_license: Option<&str> = None;
    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix('#') else {
            break;
        };
        let comment_trimmed = comment.trim();
        if let Some(spdx) = comment_trimmed.strip_prefix("SPDX-License-Identifier:") {
            spdx_license = Some(spdx.trim());
        } else if comment_trimmed
            .to_ascii_lowercase()
            .starts_with("copyright")
        {
            copyrights.insert(comment_trimmed.to_owned());
        }
    }
    match spdx_license {
        Some("BSD-3-Clause-Clear") => Ok(()),
        Some(other) => Err(format!(
            "unexpected SPDX-License-Identifier `{other}` in {} (expected BSD-3-Clause-Clear)",
            path.display()
        )
        .into()),
        None => Err(format!(
            "missing `# SPDX-License-Identifier:` header in {}",
            path.display()
        )
        .into()),
    }
}

fn csr_hpm_num(csr_name: &str) -> Option<String> {
    let digits: String = csr_name.chars().filter(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty() && csr_name.contains("hpm")).then_some(digits)
}

fn collect_yaml_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
            out.push(path);
        }
    }
    Ok(())
}

fn extract_defined_by(val: Option<&Value>) -> Vec<String> {
    let Some(val) = val else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_ext_names(val, &mut out);
    out
}

fn collect_ext_names(val: &Value, out: &mut Vec<String>) {
    if let Some(map) = val.as_mapping() {
        if let Some(ext) = map.get(Value::String("extension".to_owned()))
            && let Some(name) = ext.get("name").and_then(Value::as_str)
        {
            let lower = name.to_ascii_lowercase();
            if !out.contains(&lower) {
                out.push(lower);
            }
        }
        if let Some(arr) = map
            .get(Value::String("anyOf".to_owned()))
            .and_then(Value::as_sequence)
        {
            let mut alts = Vec::new();
            for item in arr {
                let mut sub = Vec::new();
                collect_ext_names(item, &mut sub);
                alts.extend(sub);
            }
            if !alts.is_empty() {
                let joined = alts.join("|");
                if !out.contains(&joined) {
                    out.push(joined);
                }
            }
        }
        if let Some(arr) = map
            .get(Value::String("allOf".to_owned()))
            .and_then(Value::as_sequence)
        {
            for item in arr {
                collect_ext_names(item, out);
            }
        }
    }
}

fn field_bits_str(v: &Value) -> String {
    for key in ["location", "location_rv64", "location_rv32"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return s.to_owned();
        }
        if let Some(n) = v.get(key).and_then(Value::as_u64) {
            return n.to_string();
        }
    }
    String::new()
}

pub fn clean_text(raw: &str) -> String {
    clean_text_with_hpm(raw, None)
}

fn clean_text_with_hpm(raw: &str, hpm_num: Option<&str>) -> String {
    let stripped_erb = strip_erb_tags(raw);
    let decoded = decode_html_entities(&stripped_erb);
    let cleaned_adoc = clean_asciidoc(&decoded);
    let no_pp = strip_asciidoc_passthrough(&cleaned_adoc);
    let no_attrs = strip_stray_attribute_refs(&no_pp, hpm_num);
    collapse_blank_lines(&no_attrs)
}

fn strip_asciidoc_passthrough(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("++") {
        let prev_is_alnum = rest[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if prev_is_alnum {
            out.push_str(&rest[..pos + 2]);
            rest = &rest[pos + 2..];
            continue;
        }
        let after = &rest[pos + 2..];
        if let Some(close) = after.find("++")
            && !after[..close].contains('\n')
        {
            out.push_str(&rest[..pos]);
            out.push_str(&after[..close]);
            rest = &after[close + 2..];
        } else {
            out.push_str(&rest[..pos + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn strip_stray_attribute_refs(input: &str, hpm_num: Option<&str>) -> String {
    let substituted = if let Some(n) = hpm_num {
        input.replace("{hpm_num}", n)
    } else {
        input.to_owned()
    };
    let mut out = String::with_capacity(substituted.len());
    let mut rest = substituted.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let prev_boundary = rest[..open]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace());
        if let Some(close) = after.find('}')
            && !after[..close].contains(['\n', '{', ' '])
            && prev_boundary
        {
            rest = &after[close + 1..];
            continue;
        }
        out.push('{');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn contains_todo_word(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut idx = 0;
    while let Some(pos) = s[idx..].find("TODO") {
        let abs = idx + pos;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric() && bytes[abs - 1] != b'_';
        let right = abs + 4;
        let right_ok =
            right >= bytes.len() || !bytes[right].is_ascii_alphanumeric() && bytes[right] != b'_';
        if left_ok && right_ok {
            return true;
        }
        idx = right;
    }
    false
}

fn strip_todo_sentences(
    owner: &str,
    text: Option<String>,
    cleaned_todos: &mut Vec<String>,
) -> Option<String> {
    let raw = text?;
    if !contains_todo_word(&raw) {
        return Some(raw);
    }
    cleaned_todos.push(owner.to_owned());

    let mut kept_paras = Vec::new();
    for para in raw.split("\n\n") {
        let trimmed = para.trim();
        if trimmed.is_empty() || !contains_todo_word(trimmed) {
            if !trimmed.is_empty() {
                kept_paras.push(trimmed.to_owned());
            }
            continue;
        }
        // Drop standalone lines containing TODO; within a paragraph drop individual TODO sentences.
        let mut kept_lines = Vec::new();
        for line in trimmed.lines() {
            if !contains_todo_word(line) {
                kept_lines.push(line.to_owned());
                continue;
            }
            let cleaned_line = filter_todo_sentences_in_line(line);
            if !cleaned_line.is_empty() {
                kept_lines.push(cleaned_line);
            }
        }
        let rejoined = kept_lines.join("\n").trim().to_owned();
        if !rejoined.is_empty() {
            kept_paras.push(rejoined);
        }
    }

    let final_text = kept_paras.join("\n\n").trim().to_owned();
    if final_text.is_empty() {
        None
    } else {
        Some(final_text)
    }
}

fn filter_todo_sentences_in_line(line: &str) -> String {
    let mut kept = Vec::new();
    let mut start = 0usize;
    let bytes = line.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'.' && (idx + 1 >= bytes.len() || bytes[idx + 1].is_ascii_whitespace()) {
            let sentence = line[start..=idx].trim();
            if !sentence.is_empty() && !contains_todo_word(sentence) {
                kept.push(sentence);
            }
            start = idx + 1;
        }
    }
    let tail = line[start..].trim();
    if !tail.is_empty() && !contains_todo_word(tail) {
        kept.push(tail);
    }
    kept.join(" ")
}

fn strip_erb_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<%") {
        let before = &rest[..start];
        // For block trim `<%-`, strip trailing horizontal spaces before it on the line
        let is_trim_open = rest[start..].starts_with("<%-");
        if is_trim_open {
            out.push_str(before.trim_end_matches([' ', '\t']));
        } else {
            out.push_str(before);
        }
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("%>") {
            let tag_inner = &after_open[..end];
            if tag_inner.trim_start().starts_with('=') {
                // Keep a readable placeholder for inline value interpolation
                let expr = tag_inner.trim_start().trim_start_matches('=').trim();
                if !expr.is_empty() {
                    out.push('`');
                    out.push_str(expr);
                    out.push('`');
                }
            }
            let after_close = &after_open[end + 2..];
            if tag_inner.ends_with('-') {
                rest = after_close.trim_start_matches([' ', '\t']);
                if let Some(r) = rest.strip_prefix('\n') {
                    rest = r;
                }
            } else {
                rest = after_close;
            }
        } else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&ne;", "≠")
        .replace("&#8800;", "≠")
        .replace("&infin;", "∞")
        .replace("&pm;", "±")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
        .replace("&le;", "≤")
        .replace("&ge;", "≥")
        .replace("&times;", "×")
        .replace("&minus;", "−")
}

fn clean_asciidoc(input: &str) -> String {
    // 1. Convert or replace AsciiDoc tables (`|===` / `!===`) before line-level cleanup
    let with_tables = process_asciidoc_tables(input);

    // 2. Clean inline markup line by line
    let mut lines = Vec::new();
    for raw_line in with_tables.lines() {
        let trimmed = raw_line.trim();
        // Drop standalone AsciiDoc anchor lines like `[[rm]]`, `[[fcsr, ...]]`
        // while keeping inline syntax like `MULH[[S]U]`.
        if trimmed.starts_with("[[")
            && trimmed.ends_with("]]")
            && !trimmed[2..trimmed.len() - 2].contains("[[")
        {
            continue;
        }
        if trimmed == "----" || trimmed == "====" {
            continue;
        }
        if trimmed == "[NOTE]" || trimmed == "[TIP]" || trimmed == "[IMPORTANT]" {
            lines.push("**Note:**".to_owned());
            continue;
        }

        let mut line = raw_line.trim_end().to_owned();
        if let Some(stripped) = line.strip_suffix(" +") {
            line = stripped.to_owned();
        }
        line = replace_latexmath(&line);
        line = replace_xrefs(&line);
        line = replace_adoc_crossrefs(&line);
        line = line.replace("[.rotate]#", "").replace('#', "");
        lines.push(line);
    }

    lines.join("\n")
}

fn process_asciidoc_tables(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "|===" || trimmed == "!===" {
            let delim = trimmed;
            let sep = if delim == "|===" { '|' } else { '!' };
            // Strip preceding `[%...]` table attribute line and optional `.Title` / `[[anchor]]`
            while let Some(prev) = out.last() {
                let pt = prev.trim();
                if (pt.starts_with('[')
                    && pt.ends_with(']')
                    && (pt.contains('%') || pt.contains("cols=")))
                    || (pt.starts_with('.') && pt.len() > 1 && !pt.starts_with(".."))
                    || (pt.starts_with("[[") && pt.ends_with("]]"))
                {
                    out.pop();
                } else {
                    break;
                }
            }

            // Collect body until matching closing delimiter
            let mut j = i + 1;
            let mut body_lines = Vec::new();
            while j < lines.len() && lines[j].trim() != delim {
                body_lines.push(lines[j]);
                j += 1;
            }
            if j < lines.len() {
                j += 1;
            }

            if let Some(md_table) = try_convert_simple_table(&body_lines, sep) {
                out.extend(md_table);
            } else {
                out.push("*See the RISC-V ISA specification for the full table.*".to_owned());
            }
            i = j;
            continue;
        }
        out.push(lines[i].to_owned());
        i += 1;
    }

    out.join("\n")
}

fn try_convert_simple_table(body_lines: &[&str], sep: char) -> Option<Vec<String>> {
    let joined = body_lines.join("\n");
    if joined.contains("[.rotate]") || joined.contains(".2+") || joined.contains(".3+") {
        return None;
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    for raw in body_lines {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with(sep) {
            return None;
        }
        let mut cells = Vec::new();
        for cell in t[sep.len_utf8()..].split(sep) {
            let cleaned = replace_adoc_crossrefs(&replace_xrefs(&replace_latexmath(cell.trim())));
            cells.push(cleaned.replace('\n', " "));
        }
        if cells.len() < 2 {
            return None;
        }
        rows.push(cells);
    }

    let col_count = rows.first()?.len();
    if rows.len() < 2 || !rows.iter().all(|r| r.len() == col_count) {
        return None;
    }

    let mut md = Vec::with_capacity(rows.len() + 1);
    for (idx, row) in rows.iter().enumerate() {
        let escaped: Vec<String> = row.iter().map(|c| c.replace('|', "\\|")).collect();
        md.push(format!("| {} |", escaped.join(" | ")));
        if idx == 0 {
            let sep_cols = vec!["---"; col_count].join(" | ");
            md.push(format!("| {sep_cols} |"));
        }
    }
    Some(md)
}

fn replace_latexmath(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("latexmath:[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "latexmath:[".len()..];
        if let Some(end) = after.find(']') {
            let inner = &after[..end];
            let cleaned = inner
                .trim_matches('$')
                .replace("\\infty", "∞")
                .replace("\\times", "×")
                .replace("\\pm", "±")
                .replace("\\le", "≤")
                .replace("\\ge", "≥")
                .replace("\\ne", "≠")
                .replace("\\log_2", "log2")
                .replace("\\_", "_")
                .replace(['{', '}'], "");
            out.push_str(cleaned.trim());
            rest = &after[end + 1..];
        } else {
            out.push_str("latexmath:[");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn replace_xrefs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("xref:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "xref:".len()..];
        if let Some(lb) = after.find('[')
            && let Some(rb) = after[lb + 1..].find(']')
        {
            let target = after[..lb].trim();
            let label = after[lb + 1..lb + 1 + rb].trim();
            if label.is_empty() {
                out.push_str(target);
            } else {
                out.push_str(label);
            }
            rest = &after[lb + 1 + rb + 1..];
        } else {
            out.push_str("xref:");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn replace_adoc_crossrefs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<<") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find(">>") {
            let inner = &after[..end];
            if let Some((_, label)) = inner.split_once(',') {
                out.push_str(label.trim());
            } else {
                out.push_str(inner.trim());
            }
            rest = &after[end + 2..];
        } else {
            out.push_str("<<");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn collapse_blank_lines(input: &str) -> String {
    let lines: Vec<&str> = input.lines().map(str::trim_end).collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut prev_blank = true;
    for line in lines {
        let blank = line.trim().is_empty();
        if blank {
            if !prev_blank {
                out.push("");
            }
            prev_blank = true;
        } else {
            out.push(line);
            prev_blank = false;
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_removes_asciidoc_tables_latexmath_and_xrefs() {
        let simple_table = "\
[[norm:example]]
Computes `latexmath:[$x \\times y$]` with `xref:insns-add[ADD]` and <<#sec-mul, Mul Section>>.

|===
| Bit | Meaning
| 0 | Infinity
| 1 | Subnormal
|===
";
        let cleaned = clean_text(simple_table);
        assert!(!cleaned.contains("|==="), "leftover table fence: {cleaned}");
        assert!(
            !cleaned.contains("latexmath:"),
            "leftover latexmath: {cleaned}"
        );
        assert!(!cleaned.contains("xref:"), "leftover xref: {cleaned}");
        assert!(!cleaned.contains("[[norm:"), "leftover anchor: {cleaned}");
        assert!(cleaned.contains("| Bit | Meaning |"));
        assert!(cleaned.contains("| --- | --- |"));
        assert!(cleaned.contains("ADD"));
        assert!(cleaned.contains("Mul Section"));

        let complex_table = "\
[%autowidth,%footer]
|===
2+| Field
| [.rotate]#XS# | Status
|===
";
        let cleaned_complex = clean_text(complex_table);
        assert!(!cleaned_complex.contains("|==="));
        assert!(!cleaned_complex.contains("[.rotate]"));
        assert!(cleaned_complex.contains("See the RISC-V ISA specification"));
    }

    #[test]
    fn clean_text_preserves_cpp_resolves_hpm_num_and_drops_todo_sentences() {
        let sc = clean_text("Sequentially consistent in the C++ memory_order_seq_cst sense.");
        assert!(sc.contains("C++ memory_order_seq_cst"), "damaged C++: {sc}");

        let pp = clean_text("Value ++x[0]++ stays readable while C++ stays intact.");
        assert!(pp.contains("x[0]"), "passthrough not stripped: {pp}");
        assert!(pp.contains("C++"), "C++ damaged: {pp}");

        let hpm = clean_text_with_hpm(
            "Alias of `mhpmevent{hpm_num}`[63:32]. Preserves `vset{i}vl{i}` and drops {stray}.",
            Some("3"),
        );
        assert!(
            hpm.contains("`mhpmevent3`"),
            "hpm_num not substituted: {hpm}"
        );
        assert!(
            hpm.contains("`vset{i}vl{i}`"),
            "mnemonic template damaged: {hpm}"
        );
        assert!(!hpm.contains("{stray}"), "stray attr not stripped: {hpm}");

        let mut todos = Vec::new();
        let kept = strip_todo_sentences(
            "mtinst",
            Some("First sentence is useful. The values are documented in TODO.\n\nSecond paragraph stays.".to_owned()),
            &mut todos,
        )
        .expect("non-empty kept");
        assert!(kept.contains("First sentence is useful."));
        assert!(kept.contains("Second paragraph stays."));
        assert!(!kept.contains("TODO"), "TODO remained: {kept}");
        assert_eq!(todos, vec!["mtinst".to_owned()]);

        let dropped = strip_todo_sentences("mncause.CODE", Some("TODO".to_owned()), &mut todos);
        assert!(dropped.is_none());
    }

    #[test]
    fn collects_udb_copyrights_and_rejects_unexpected_spdx() {
        let ok_yaml = "\
# Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
# SPDX-License-Identifier: BSD-3-Clause-Clear

name: add
";
        let mut holders = BTreeSet::new();
        collect_license_headers(Path::new("add.yaml"), ok_yaml, &mut holders)
            .expect("BSD-3-Clause-Clear passes");
        assert!(
            holders.contains("Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.")
        );

        let wrong_spdx = "# SPDX-License-Identifier: MIT\nname: foo\n";
        let err = collect_license_headers(Path::new("foo.yaml"), wrong_spdx, &mut holders)
            .expect_err("non-BSD-3-Clause-Clear should fail");
        assert!(err.to_string().contains("foo.yaml"));

        let missing_spdx = "# Copyright (c) Example\nname: bar\n";
        let err2 = collect_license_headers(Path::new("bar.yaml"), missing_spdx, &mut holders)
            .expect_err("missing SPDX should fail");
        assert!(err2.to_string().contains("bar.yaml"));
    }
}
