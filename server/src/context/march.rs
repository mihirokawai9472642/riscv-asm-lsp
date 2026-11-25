use std::collections::{BTreeSet, VecDeque};

use crate::data::{Xlen, extension_graph};

const COMPRESSED_SUBEXTENSIONS: &[&str] = &["c", "zca", "zcf", "zcd", "zcb", "zcmp", "zcmt"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarchSpec {
    pub raw: String,
    pub xlen: Xlen,
    pub extensions: BTreeSet<String>,
    pub enable_experimental: bool,
}

impl MarchSpec {
    pub fn parse(raw: &str) -> Self {
        Self::parse_with_experimental(raw, false)
    }

    pub fn parse_with_experimental(raw: &str, enable_experimental: bool) -> Self {
        let lower = raw.trim().to_ascii_lowercase();
        let (xlen, rest) = if let Some(r) = lower.strip_prefix("rv64") {
            (Xlen::Rv64, r)
        } else if let Some(r) = lower.strip_prefix("rv32") {
            (Xlen::Rv32, r)
        } else {
            (Xlen::Both, lower.as_str())
        };

        let mut exts = BTreeSet::new();
        let mut parts = rest.split('_');
        if let Some(base_and_single) = parts.next() {
            parse_single_letter_exts(base_and_single, &mut exts);
        }
        for part in parts {
            let ext = strip_ext_version(part);
            if !ext.is_empty() {
                exts.insert(ext.to_owned());
            }
        }

        let mut spec = Self {
            raw: raw.trim().to_owned(),
            xlen,
            extensions: exts,
            enable_experimental,
        };
        spec.expand_implications();
        spec
    }

    pub fn apply_rvc(&mut self, enable: bool) {
        if enable {
            self.extensions.insert("c".to_owned());
            self.expand_implications();
        } else {
            for ext in COMPRESSED_SUBEXTENSIONS {
                self.extensions.remove(*ext);
            }
        }
    }

    pub fn apply_option_arch(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            return;
        }
        let lower = trimmed.to_ascii_lowercase();
        for token in lower.split(',') {
            let t = token.trim();
            if let Some(ext) = t.strip_prefix('+') {
                let name = strip_ext_version(ext);
                if !name.is_empty() {
                    self.extensions.insert(name.to_owned());
                    self.expand_implications();
                }
            } else if let Some(ext) = t.strip_prefix('-') {
                let name = strip_ext_version(ext);
                self.remove_extension(name);
            } else if t.starts_with("rv32") || t.starts_with("rv64") {
                *self = Self::parse_with_experimental(t, self.enable_experimental);
            }
        }
    }

    fn remove_extension(&mut self, name: &str) {
        if name == "c" {
            self.apply_rvc(false);
            return;
        }
        let graph = extension_graph();
        let mut queue = vec![name.to_owned()];
        while let Some(cur) = queue.pop() {
            if !self.extensions.remove(&cur) {
                continue;
            }
            if let Some(dependents) = graph.reverse_implies.get(&cur) {
                for dependent in dependents {
                    if self.extensions.contains(dependent) {
                        queue.push(dependent.clone());
                    }
                }
            }
        }
    }

    fn expand_implications(&mut self) {
        let graph = extension_graph();
        if !self.enable_experimental {
            self.extensions.retain(|e| !graph.experimental.contains(e));
        }

        let mut queue: VecDeque<String> = self.extensions.iter().cloned().collect();
        while let Some(cur) = queue.pop_front() {
            let Some(implied) = graph.implies.get(&cur) else {
                continue;
            };
            for dep in implied {
                if !self.enable_experimental && graph.experimental.contains(dep) {
                    continue;
                }
                if self.extensions.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    pub fn supports_xlen(&self, req: Xlen) -> bool {
        matches!(
            (self.xlen, req),
            (_, Xlen::Both) | (Xlen::Both, _) | (Xlen::Rv32, Xlen::Rv32) | (Xlen::Rv64, Xlen::Rv64)
        )
    }

    pub fn supports_extensions(&self, reqs: &[String], xlen: Xlen) -> bool {
        if !self.supports_xlen(xlen) {
            return false;
        }
        reqs.iter().all(|req| {
            req.split('|')
                .any(|alt| self.extensions.contains(&alt.to_ascii_lowercase()))
        })
    }

    pub fn supports_csr(&self, reqs: &[String], xlen: Xlen) -> bool {
        if !self.supports_xlen(xlen) {
            return false;
        }
        reqs.iter().all(|req| {
            req.split('|').any(|alt| {
                let lower = alt.to_ascii_lowercase();
                // Privileged levels (M/S/U) and base counter CSRs are accepted regardless of -march.
                matches!(lower.as_str(), "sm" | "s" | "u" | "zicntr" | "zihpm")
                    || self.extensions.contains(&lower)
            })
        })
    }

    /// Format active extensions for `llvm-mc -mattr=...`.
    pub fn to_llvm_mattr(&self) -> String {
        let mut attrs: Vec<String> = self
            .extensions
            .iter()
            .filter(|e| e.as_str() != "i" && e.as_str() != "g")
            .map(|e| format!("+{e}"))
            .collect();
        if !self.extensions.contains("c") {
            attrs.push("-c".to_owned());
            if !self.extensions.contains("zca") {
                attrs.push("-zca".to_owned());
            }
        }
        attrs.join(",")
    }

    pub fn llvm_triple(&self) -> &'static str {
        match self.xlen {
            Xlen::Rv32 => "riscv32",
            Xlen::Rv64 | Xlen::Both => "riscv64",
        }
    }
}

fn parse_single_letter_exts(segment: &str, out: &mut BTreeSet<String>) {
    // Strip trailing version like `2p1` while collecting single-letter ISA flags.
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() {
            // If `z`, `s`, or `x` starts a multi-letter extension without `_`, read rest
            if (b == b'z' || b == b's' || b == b'x')
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_alphabetic()
            {
                let ext = strip_ext_version(&segment[i..]);
                if !ext.is_empty() {
                    out.insert(ext.to_owned());
                }
                break;
            }
            out.insert((b as char).to_string());
            i += 1;
            // Skip version digits / `p` (e.g. `i2p1`)
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'p') {
                // Stop skipping `p` if it's not followed by a digit (unless `p` extension itself)
                if bytes[i] == b'p' && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    break;
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

fn strip_ext_version(token: &str) -> &str {
    // Strip trailing `[0-9]+p[0-9]+` (or trailing digits) from extension names like `zba1p0`
    let bytes = token.as_bytes();
    let mut end = bytes.len();
    let mut i = end;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i < end && i > 0 && bytes[i - 1] == b'p' {
        let mut j = i - 1;
        while j > 0 && bytes[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i - 1 {
            end = j;
        }
    }
    &token[..end]
}

/// Scan `.option` directives up to `target_line` (0-based) to determine effective
/// `-march` state and `.option` directives to replay for `llvm-mc`.
pub fn effective_options_at_line(
    text: &str,
    target_line: usize,
    base: &MarchSpec,
) -> (MarchSpec, Vec<String>) {
    let mut current = base.clone();
    let mut stack: Vec<(MarchSpec, Vec<String>)> = Vec::new();
    let mut active_lines: Vec<String> = Vec::new();

    let mut in_block = false;
    for (line_idx, raw_line) in text.lines().enumerate() {
        if line_idx >= target_line {
            break;
        }
        let stripped = crate::line::strip_comments_with_state(raw_line, &mut in_block, false);
        for stmt in crate::line::split_unquoted_statements_stripped(&stripped) {
            let trimmed = stmt.trim();
            let Some(rest) = trimmed.strip_prefix(".option") else {
                continue;
            };
            if rest.chars().next().is_some_and(|c| !c.is_whitespace()) {
                continue;
            }
            let arg = rest.trim();
            let lower = arg.to_ascii_lowercase();
            if lower == "push" {
                stack.push((current.clone(), active_lines.clone()));
            } else if lower == "pop" {
                if let Some((prev_spec, prev_lines)) = stack.pop() {
                    current = prev_spec;
                    active_lines = prev_lines;
                }
            } else if lower == "rvc" {
                current.apply_rvc(true);
                active_lines.push(trimmed.to_owned());
            } else if lower == "norvc" {
                current.apply_rvc(false);
                active_lines.push(trimmed.to_owned());
            } else if let Some(arch_arg) = lower.strip_prefix("arch")
                && let Some(arch_val) = arch_arg.trim_start().strip_prefix(',')
            {
                current.apply_option_arch(arch_val);
                active_lines.push(trimmed.to_owned());
            }
        }
    }

    (current, active_lines)
}
