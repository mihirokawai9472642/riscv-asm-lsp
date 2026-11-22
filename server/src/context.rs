use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use lsp_types::Uri;
use serde::Deserialize;

use crate::config::Config;
use crate::data::Xlen;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarchSpec {
    pub raw: String,
    pub xlen: Xlen,
    pub extensions: BTreeSet<String>,
}

impl MarchSpec {
    pub fn parse(raw: &str) -> Self {
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
        };
        spec.expand_implications();
        spec
    }

    pub fn apply_rvc(&mut self, enable: bool) {
        if enable {
            self.extensions.insert("c".to_owned());
            self.expand_implications();
        } else {
            for ext in ["c", "zca", "zcf", "zcd"] {
                self.extensions.remove(ext);
            }
        }
    }

    pub fn apply_option_arch(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            return;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("rv32") || lower.starts_with("rv64") {
            *self = Self::parse(&lower);
            return;
        }
        for token in lower.split(',') {
            let t = token.trim();
            if let Some(ext) = t.strip_prefix('+') {
                let name = strip_ext_version(ext);
                if !name.is_empty() {
                    self.extensions.insert(name.to_owned());
                }
            } else if let Some(ext) = t.strip_prefix('-') {
                let name = strip_ext_version(ext);
                if name == "c" {
                    self.apply_rvc(false);
                } else {
                    self.extensions.remove(name);
                }
            } else if t.starts_with("rv32") || t.starts_with("rv64") {
                *self = Self::parse(t);
            }
        }
        self.expand_implications();
    }

    fn expand_implications(&mut self) {
        if self.extensions.contains("g") {
            for ext in ["i", "m", "a", "f", "d", "zicsr", "zifencei"] {
                self.extensions.insert(ext.to_owned());
            }
        }
        if self.extensions.contains("m") {
            self.extensions.insert("zmmul".to_owned());
        }
        if self.extensions.contains("a") {
            self.extensions.insert("zaamo".to_owned());
            self.extensions.insert("zalrsc".to_owned());
        }
        if self.extensions.contains("d") {
            self.extensions.insert("f".to_owned());
        }
        if self.extensions.contains("f") {
            self.extensions.insert("zicsr".to_owned());
        }
        if self.extensions.contains("c") {
            self.extensions.insert("zca".to_owned());
            if self.extensions.contains("d") {
                self.extensions.insert("zcd".to_owned());
            }
            if self.xlen == Xlen::Rv32 && self.extensions.contains("f") {
                self.extensions.insert("zcf".to_owned());
            }
        }
        if self.extensions.contains("b") {
            for ext in ["zba", "zbb", "zbs"] {
                self.extensions.insert(ext.to_owned());
            }
        }
        if self.extensions.contains("v") {
            for ext in [
                "zve32x", "zve32f", "zve64x", "zve64f", "zve64d", "zvl32b", "zvl64b", "zvl128b",
            ] {
                self.extensions.insert(ext.to_owned());
            }
        }
        if self.extensions.contains("zfh") {
            self.extensions.insert("zfhmin".to_owned());
        }
        if self.extensions.contains("zk") {
            for ext in ["zkn", "zkr", "zkt"] {
                self.extensions.insert(ext.to_owned());
            }
        }
        if self.extensions.contains("zkn") {
            for ext in ["zbkb", "zbkc", "zbkx", "zkne", "zknd", "zknh"] {
                self.extensions.insert(ext.to_owned());
            }
        }
        if self.extensions.contains("zks") {
            for ext in ["zbkb", "zbkc", "zbkx", "zksed", "zksh"] {
                self.extensions.insert(ext.to_owned());
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
            // If `z` or `s` starts a multi-letter extension without `_` (rare), read rest
            if (b == b'z' || b == b's') && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic()
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

    for (line_idx, raw_line) in text.lines().enumerate() {
        if line_idx >= target_line {
            break;
        }
        let code = strip_line_comment(raw_line).trim();
        for stmt in code.split(';') {
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

fn strip_line_comment(line: &str) -> &str {
    // Respect string quotes minimally; assembly `.option` lines don't usually carry quotes
    if let Some(pos) = line.find('#') {
        &line[..pos]
    } else if let Some(pos) = line.find("//") {
        &line[..pos]
    } else {
        line
    }
}

#[derive(Debug, Clone)]
pub struct FileContext {
    pub target: String,
    pub march: MarchSpec,
    pub clang_flags: Vec<String>,
    pub include_dirs: Vec<PathBuf>,
    pub quote_dirs: Vec<PathBuf>,
    pub working_dir: Option<PathBuf>,
    pub client_root: Option<PathBuf>,
    pub canon_root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RawCompileCommand {
    directory: Option<String>,
    file: String,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedCompileEntry {
    directory: PathBuf,
    clang_flags: Vec<String>,
    target: Option<String>,
    march: Option<String>,
    include_dirs: Vec<PathBuf>,
    quote_dirs: Vec<PathBuf>,
}

pub struct CompileContext {
    workspace_root: Option<PathBuf>,
    config: Config,
    entries: HashMap<PathBuf, ParsedCompileEntry>,
    loaded_db_path: Option<PathBuf>,
    loaded_mtime: Option<SystemTime>,
}

impl CompileContext {
    pub fn new(workspace_root: Option<PathBuf>, config: Config) -> Self {
        let mut ctx = Self {
            workspace_root,
            config,
            entries: HashMap::new(),
            loaded_db_path: None,
            loaded_mtime: None,
        };
        ctx.reload_if_needed();
        ctx
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn reload_if_needed(&mut self) {
        let Some(root) = &self.workspace_root else {
            return;
        };
        let mut found: Option<(PathBuf, SystemTime)> = None;
        for rel in &self.config.compile_commands_dirs {
            let candidate = if rel.is_empty() {
                root.join("compile_commands.json")
            } else {
                root.join(rel).join("compile_commands.json")
            };
            if let Ok(meta) = fs::metadata(&candidate) {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                found = Some((candidate, mtime));
                break;
            }
        }

        let Some((db_path, mtime)) = found else {
            self.entries.clear();
            self.loaded_db_path = None;
            self.loaded_mtime = None;
            return;
        };

        if self.loaded_db_path.as_ref() == Some(&db_path) && self.loaded_mtime == Some(mtime) {
            return;
        }

        if let Ok(text) = fs::read_to_string(&db_path)
            && let Ok(raw_list) = serde_json::from_str::<Vec<RawCompileCommand>>(&text)
        {
            let mut entries = HashMap::new();
            for item in raw_list {
                let dir = item
                    .directory
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .map(|p| lexical_normalize_path(&p))
                    .unwrap_or_else(|| lexical_normalize_path(root));
                let file_path = resolve_path(&dir, &item.file);
                let args = if let Some(args) = item.arguments {
                    args
                } else if let Some(cmd) = item.command {
                    split_command_args(&cmd)
                } else {
                    Vec::new()
                };
                let parsed = extract_relevant_flags(&dir, &args);
                entries.insert(file_path, parsed);
            }
            self.entries = entries;
            self.loaded_db_path = Some(db_path);
            self.loaded_mtime = Some(mtime);
        }
    }

    pub fn for_uri(&mut self, uri: &Uri) -> FileContext {
        self.reload_if_needed();
        if let Some(path) = uri_to_path(uri) {
            let norm = normalize_path(&path);
            if let Some(entry) = self.entries.get(&norm).or_else(|| self.entries.get(&path)) {
                let target = entry
                    .target
                    .clone()
                    .unwrap_or_else(|| self.config.default_target.clone());
                let march_str = entry.march.as_deref().unwrap_or(&self.config.default_march);
                let mut clang_flags = entry.clang_flags.clone();
                if entry.target.is_none() {
                    clang_flags.push(format!("--target={target}"));
                }
                if entry.march.is_none() {
                    clang_flags.push(format!("-march={march_str}"));
                }
                return FileContext {
                    target,
                    march: MarchSpec::parse(march_str),
                    clang_flags,
                    include_dirs: entry.include_dirs.clone(),
                    quote_dirs: entry.quote_dirs.clone(),
                    working_dir: Some(entry.directory.clone()),
                    client_root: self.workspace_root.clone(),
                    canon_root: self.workspace_root.as_deref().map(normalize_path),
                };
            }
        }

        let target = self.config.default_target.clone();
        let march_str = self.config.default_march.clone();
        FileContext {
            target: target.clone(),
            march: MarchSpec::parse(&march_str),
            clang_flags: vec![format!("--target={target}"), format!("-march={march_str}")],
            include_dirs: Vec::new(),
            quote_dirs: Vec::new(),
            working_dir: self.workspace_root.clone(),
            client_root: self.workspace_root.clone(),
            canon_root: self.workspace_root.as_deref().map(normalize_path),
        }
    }
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri.as_str()).ok()?;
    parsed.to_file_path().ok()
}

/// Convert a filesystem path to a `file://` URI without dereferencing symlinks.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let abs = if path.is_absolute() {
        lexical_normalize_path(path)
    } else {
        let cwd = std::env::current_dir().ok()?;
        lexical_normalize_path(&cwd.join(path))
    };
    let url = url::Url::from_file_path(&abs).ok()?;
    url.as_str().parse().ok()
}

/// Map a path back under the client's workspace root when it lives inside the workspace,
/// avoiding exposing canonicalized symlink targets in LSP responses.
pub fn display_path_for_client(
    path: &Path,
    client_root: Option<&Path>,
    canon_root: Option<&Path>,
) -> PathBuf {
    if let (Some(c_root), Some(k_root)) = (client_root, canon_root) {
        let norm_p = normalize_path(path);
        if let Ok(rel) = norm_p.strip_prefix(k_root) {
            return lexical_normalize_path(&c_root.join(rel));
        }
    }
    lexical_normalize_path(path)
}

fn resolve_path(base: &Path, target: &str) -> PathBuf {
    let p = PathBuf::from(target);
    let joined = if p.is_absolute() { p } else { base.join(p) };
    normalize_path(&joined)
}

pub fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut stack: Vec<std::path::Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match stack.last() {
                Some(std::path::Component::Normal(_)) => {
                    stack.pop();
                }
                Some(std::path::Component::RootDir) | Some(std::path::Component::Prefix(_)) => {}
                _ => stack.push(comp),
            },
            other => stack.push(other),
        }
    }
    stack.iter().collect()
}

pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    lexical_normalize_path(path)
}

fn extract_relevant_flags(dir: &Path, args: &[String]) -> ParsedCompileEntry {
    let mut clang_flags = Vec::new();
    let mut target = None;
    let mut march = None;
    let mut include_dirs = Vec::new();
    let mut quote_dirs = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if i == 0 && !arg.starts_with('-') {
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--target=") {
            target = Some(val.to_owned());
            clang_flags.push(arg.clone());
        } else if (arg == "--target" || arg == "-target") && i + 1 < args.len() {
            let val = args[i + 1].clone();
            target = Some(val.clone());
            clang_flags.push(format!("--target={val}"));
            i += 1;
        } else if let Some(val) = arg.strip_prefix("-march=") {
            march = Some(val.to_owned());
            clang_flags.push(arg.clone());
        } else if arg.starts_with("-mabi=") {
            clang_flags.push(arg.clone());
        } else if arg == "-mabi" && i + 1 < args.len() {
            clang_flags.push(arg.clone());
            clang_flags.push(args[i + 1].clone());
            i += 1;
        } else if let Some(p) = arg.strip_prefix("-I") {
            let path = if p.is_empty() && i + 1 < args.len() {
                i += 1;
                resolve_path(dir, &args[i])
            } else {
                resolve_path(dir, p)
            };
            include_dirs.push(path.clone());
            clang_flags.push("-I".to_owned());
            clang_flags.push(path.to_string_lossy().into_owned());
        } else if let Some(p) = arg.strip_prefix("-iquote") {
            let path = if p.is_empty() && i + 1 < args.len() {
                i += 1;
                resolve_path(dir, &args[i])
            } else {
                resolve_path(dir, p)
            };
            quote_dirs.push(path.clone());
            clang_flags.push("-iquote".to_owned());
            clang_flags.push(path.to_string_lossy().into_owned());
        } else if let Some(p) = arg.strip_prefix("-isystem") {
            let path = if p.is_empty() && i + 1 < args.len() {
                i += 1;
                resolve_path(dir, &args[i])
            } else {
                resolve_path(dir, p)
            };
            include_dirs.push(path.clone());
            clang_flags.push("-isystem".to_owned());
            clang_flags.push(path.to_string_lossy().into_owned());
        } else if let Some(f) = arg.strip_prefix("-include") {
            let raw = if f.is_empty() && i + 1 < args.len() {
                i += 1;
                args[i].as_str()
            } else {
                f
            };
            if !raw.is_empty() {
                let path = resolve_path(dir, raw);
                clang_flags.push("-include".to_owned());
                clang_flags.push(path.to_string_lossy().into_owned());
            }
        } else if let Some(f) = arg.strip_prefix("-imacros") {
            let raw = if f.is_empty() && i + 1 < args.len() {
                i += 1;
                args[i].as_str()
            } else {
                f
            };
            if !raw.is_empty() {
                let path = resolve_path(dir, raw);
                clang_flags.push("-imacros".to_owned());
                clang_flags.push(path.to_string_lossy().into_owned());
            }
        } else if let Some(d) = arg.strip_prefix("--sysroot=") {
            if !d.is_empty() {
                let path = resolve_path(dir, d);
                clang_flags.push(format!("--sysroot={}", path.to_string_lossy()));
            }
        } else if arg == "--sysroot" && i + 1 < args.len() {
            i += 1;
            let path = resolve_path(dir, &args[i]);
            clang_flags.push(format!("--sysroot={}", path.to_string_lossy()));
        } else if let Some(d) = arg.strip_prefix("-isysroot") {
            let raw = if d.is_empty() && i + 1 < args.len() {
                i += 1;
                args[i].as_str()
            } else {
                d
            };
            if !raw.is_empty() {
                let path = resolve_path(dir, raw);
                clang_flags.push("-isysroot".to_owned());
                clang_flags.push(path.to_string_lossy().into_owned());
            }
        } else if let Some(wa) = arg.strip_prefix("-Wa,") {
            let wa_tokens: Vec<&str> = wa.split(',').collect();
            let mut wi = 0;
            let mut allowed_wa = Vec::new();
            while wi < wa_tokens.len() {
                let tok = wa_tokens[wi];
                if let Some(inc) = tok.strip_prefix("-I") {
                    let raw_path = if inc.is_empty() && wi + 1 < wa_tokens.len() {
                        wi += 1;
                        wa_tokens[wi]
                    } else {
                        inc
                    };
                    if !raw_path.is_empty() {
                        let p = resolve_path(dir, raw_path);
                        include_dirs.push(p.clone());
                        allowed_wa.push(format!("-I{}", p.to_string_lossy()));
                    }
                } else if let Some(m) = tok.strip_prefix("-march=") {
                    if !m.is_empty() {
                        march = Some(m.to_owned());
                        allowed_wa.push(format!("-march={m}"));
                    }
                } else if tok == "-march" && wi + 1 < wa_tokens.len() {
                    wi += 1;
                    let m = wa_tokens[wi];
                    if !m.is_empty() {
                        march = Some(m.to_owned());
                        allowed_wa.push(format!("-march={m}"));
                    }
                } else if let Some(abi) = tok.strip_prefix("-mabi=") {
                    if !abi.is_empty() {
                        allowed_wa.push(format!("-mabi={abi}"));
                    }
                } else if tok == "-mabi" && wi + 1 < wa_tokens.len() {
                    wi += 1;
                    let abi = wa_tokens[wi];
                    if !abi.is_empty() {
                        allowed_wa.push(format!("-mabi={abi}"));
                    }
                }
                wi += 1;
            }
            if !allowed_wa.is_empty() {
                clang_flags.push(format!("-Wa,{}", allowed_wa.join(",")));
            }
        } else if arg.starts_with("-D") || arg.starts_with("-U") {
            if (arg == "-D" || arg == "-U") && i + 1 < args.len() {
                clang_flags.push(arg.clone());
                clang_flags.push(args[i + 1].clone());
                i += 1;
            } else {
                clang_flags.push(arg.clone());
            }
        } else if arg == "-nostdinc" || arg == "-fno-pic" || arg == "-fpic" || arg == "-fPIC" {
            clang_flags.push(arg.clone());
        }
        i += 1;
    }

    ParsedCompileEntry {
        directory: lexical_normalize_path(dir),
        clang_flags,
        target,
        march,
        include_dirs,
        quote_dirs,
    }
}

/// Split a compile_commands `command` string into arguments without invoking a shell.
pub fn split_command_args(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = cmd.chars().peekable();
    let mut has_token = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
        } else if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        current.push(next);
                        chars.next();
                    } else {
                        current.push(ch);
                    }
                } else {
                    current.push(ch);
                }
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_single = true;
            has_token = true;
        } else if ch == '"' {
            in_double = true;
            has_token = true;
        } else if ch == '\\' {
            has_token = true;
            if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if ch.is_whitespace() {
            if has_token {
                args.push(std::mem::take(&mut current));
                has_token = false;
            }
        } else {
            current.push(ch);
            has_token = true;
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_march_and_filters_extensions() {
        let rv64gc = MarchSpec::parse("rv64gc");
        assert_eq!(rv64gc.xlen, Xlen::Rv64);
        assert!(rv64gc.extensions.contains("i"));
        assert!(rv64gc.extensions.contains("m"));
        assert!(rv64gc.extensions.contains("a"));
        assert!(rv64gc.extensions.contains("f"));
        assert!(rv64gc.extensions.contains("d"));
        assert!(rv64gc.extensions.contains("c"));
        assert!(rv64gc.extensions.contains("zicsr"));
        assert!(!rv64gc.extensions.contains("v"));

        let rv32i = MarchSpec::parse("rv32i2p1_zba1p0");
        assert_eq!(rv32i.xlen, Xlen::Rv32);
        assert!(rv32i.extensions.contains("i"));
        assert!(rv32i.extensions.contains("zba"));
        assert!(!rv32i.extensions.contains("m"));
        assert!(!rv32i.supports_extensions(&["m".to_owned()], Xlen::Both));
        assert!(rv32i.supports_extensions(&["zba".to_owned()], Xlen::Rv32));
        assert!(!rv32i.supports_extensions(&["zba".to_owned()], Xlen::Rv64));
    }

    #[test]
    fn option_push_pop_tracks_rvc_and_arch() {
        let src = "\
            .option push\n\
            .option norvc\n\
            nop\n\
            .option pop\n\
            .option arch, +zba\n\
            sh1add a0, a1, a2\n";
        let base = MarchSpec::parse("rv64gc");
        let (mid, _) = effective_options_at_line(src, 2, &base);
        assert!(!mid.extensions.contains("c"));
        let (after_pop, _) = effective_options_at_line(src, 5, &base);
        assert!(after_pop.extensions.contains("c"));
        assert!(after_pop.extensions.contains("zba"));
    }

    #[test]
    fn quote_aware_command_splitter_handles_flags() {
        let args = split_command_args(
            r#"clang --target=riscv64-unknown-elf -march=rv64gc -I"my dir/inc" -DNAME=\"x\" -c foo.S"#,
        );
        assert_eq!(
            args,
            vec![
                "clang",
                "--target=riscv64-unknown-elf",
                "-march=rv64gc",
                "-Imy dir/inc",
                r#"-DNAME="x""#,
                "-c",
                "foo.S",
            ]
        );
    }

    #[test]
    fn uri_conversion_handles_localhost_special_chars_and_symlinks() {
        let localhost_uri: Uri = "file://localhost/tmp/ws%20dir%2B%E6%97%A5%E6%9C%AC/a.S"
            .parse()
            .expect("uri");
        let decoded = uri_to_path(&localhost_uri).expect("file path");
        assert_eq!(decoded, PathBuf::from("/tmp/ws dir+日本/a.S"));

        let round_uri = path_to_uri(Path::new("/tmp/ws dir+日本/a.S")).expect("uri");
        assert_eq!(uri_to_path(&round_uri), Some(decoded));

        // Symlink path is preserved without canonicalization by path_to_uri & display_path_for_client
        let base =
            std::env::temp_dir().join(format!("riscv_asm_lsp_sym_ctx_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real_dir = base.join("real dir");
        let link_dir = base.join("link");
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("a.S"), "nop\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
            let linked_file = link_dir.join("a.S");
            let sym_uri = path_to_uri(&linked_file).unwrap();
            assert!(
                sym_uri.as_str().contains("/link/a.S"),
                "got {}",
                sym_uri.as_str()
            );
            let mapped = display_path_for_client(
                &fs::canonicalize(&linked_file).unwrap(),
                Some(&link_dir),
                Some(&fs::canonicalize(&real_dir).unwrap()),
            );
            assert_eq!(mapped, linked_file);
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn extract_relevant_flags_allows_sysroot_include_imacros_and_wa_options() {
        let dir = Path::new("/proj/build");
        let args: Vec<String> = [
            "clang",
            "--target=riscv64-unknown-elf",
            "-march=rv64gc",
            "-O2",
            "-include",
            "../inc/pre.h",
            "-imacros../inc/defs.h",
            "--sysroot=../sys",
            "-isysroot",
            "/opt/sdk",
            "-nostdinc",
            "-Wa,-I../asm_inc,-march=rv64gcv,--fatal-warnings",
            "-c",
            "entry.S",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        let entry = extract_relevant_flags(dir, &args);
        assert_eq!(entry.directory, PathBuf::from("/proj/build"));
        assert_eq!(entry.march.as_deref(), Some("rv64gcv"));
        assert!(entry.include_dirs.contains(&PathBuf::from("/proj/asm_inc")));
        assert!(entry.clang_flags.contains(&"-include".to_owned()));
        assert!(entry.clang_flags.contains(&"/proj/inc/pre.h".to_owned()));
        assert!(entry.clang_flags.contains(&"-imacros".to_owned()));
        assert!(entry.clang_flags.contains(&"/proj/inc/defs.h".to_owned()));
        assert!(
            entry
                .clang_flags
                .contains(&"--sysroot=/proj/sys".to_owned())
        );
        assert!(entry.clang_flags.contains(&"-isysroot".to_owned()));
        assert!(entry.clang_flags.contains(&"/opt/sdk".to_owned()));
        assert!(entry.clang_flags.contains(&"-nostdinc".to_owned()));
        assert!(
            entry
                .clang_flags
                .contains(&"-Wa,-I/proj/asm_inc,-march=rv64gcv".to_owned()),
            "flags: {:?}",
            entry.clang_flags
        );
        assert!(!entry.clang_flags.iter().any(|f| f == "-O2"));
        assert!(
            !entry
                .clang_flags
                .iter()
                .any(|f| f.contains("--fatal-warnings"))
        );
    }
}
