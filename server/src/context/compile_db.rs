use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use lsp_types::Uri;
use serde::Deserialize;

use crate::config::Config;

use super::march::MarchSpec;
use super::uri::{lexical_normalize_path, normalize_path, resolve_path, uri_to_path};

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
    pub preprocessed: bool,
}

pub fn is_linker_script_template(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".lds.s") || lower.ends_with(".ld.s")
}

pub fn is_linker_script_uri(uri: &Uri) -> bool {
    uri_to_path(uri).is_some_and(|p| is_linker_script_template(&p))
}

pub fn is_preprocessed_by_suffix(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("S" | "sx" | "h" | "H" | "hpp") => true,
        Some("s" | "inc") => false,
        _ => true,
    }
}

#[derive(Debug, Deserialize)]
struct RawCompileCommand {
    directory: Option<String>,
    file: String,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ParsedCompileEntry {
    pub directory: PathBuf,
    pub clang_flags: Vec<String>,
    pub target: Option<String>,
    pub march: Option<String>,
    pub include_dirs: Vec<PathBuf>,
    pub quote_dirs: Vec<PathBuf>,
    pub lang_override: Option<bool>,
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
        let Some((db_path, mtime)) = self.find_compile_commands(root) else {
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
            self.entries = build_compile_entries(root, raw_list);
            self.loaded_db_path = Some(db_path);
            self.loaded_mtime = Some(mtime);
        }
    }

    fn find_compile_commands(&self, root: &Path) -> Option<(PathBuf, SystemTime)> {
        for rel in &self.config.compile_commands_dirs {
            let candidate = if rel.is_empty() {
                root.join("compile_commands.json")
            } else {
                root.join(rel).join("compile_commands.json")
            };
            if let Ok(meta) = fs::metadata(&candidate) {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                return Some((candidate, mtime));
            }
        }
        None
    }

    pub fn is_preprocessed_path(&mut self, path: &Path) -> bool {
        self.reload_if_needed();
        let norm = normalize_path(path);
        if let Some(entry) = self.entries.get(&norm).or_else(|| self.entries.get(path))
            && let Some(override_pre) = entry.lang_override
        {
            return override_pre;
        }
        is_preprocessed_by_suffix(path)
    }

    pub fn for_uri(&mut self, uri: &Uri) -> FileContext {
        self.reload_if_needed();
        let path_opt = uri_to_path(uri);
        let default_pre = path_opt
            .as_deref()
            .map(is_preprocessed_by_suffix)
            .unwrap_or(true);
        if let Some(path) = path_opt {
            let norm = normalize_path(&path);
            if let Some(entry) = self.entries.get(&norm).or_else(|| self.entries.get(&path)) {
                return self.file_context_from_entry(entry, default_pre);
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
            preprocessed: default_pre,
        }
    }

    fn file_context_from_entry(
        &self,
        entry: &ParsedCompileEntry,
        default_pre: bool,
    ) -> FileContext {
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
        FileContext {
            target,
            march: MarchSpec::parse(march_str),
            clang_flags,
            include_dirs: entry.include_dirs.clone(),
            quote_dirs: entry.quote_dirs.clone(),
            working_dir: Some(entry.directory.clone()),
            client_root: self.workspace_root.clone(),
            canon_root: self.workspace_root.as_deref().map(normalize_path),
            preprocessed: entry.lang_override.unwrap_or(default_pre),
        }
    }
}

fn build_compile_entries(
    root: &Path,
    raw_list: Vec<RawCompileCommand>,
) -> HashMap<PathBuf, ParsedCompileEntry> {
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
        entries.insert(file_path, extract_relevant_flags(&dir, &args));
    }
    entries
}

pub(super) fn extract_relevant_flags(dir: &Path, args: &[String]) -> ParsedCompileEntry {
    let mut entry = ParsedCompileEntry {
        directory: lexical_normalize_path(dir),
        clang_flags: Vec::new(),
        target: None,
        march: None,
        include_dirs: Vec::new(),
        quote_dirs: Vec::new(),
        lang_override: None,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if i == 0 && !arg.starts_with('-') {
            i += 1;
            continue;
        }
        i += entry.consume_flag(dir, args, i);
    }
    entry
}

impl ParsedCompileEntry {
    fn consume_flag(&mut self, dir: &Path, args: &[String], i: usize) -> usize {
        let arg = &args[i];
        if let Some(consumed) = self.consume_language_or_arch_flag(args, i) {
            return consumed;
        }
        if let Some(consumed) = self.consume_path_flag(dir, args, i) {
            return consumed;
        }
        if let Some(wa) = arg.strip_prefix("-Wa,") {
            self.consume_wa_flags(dir, wa);
            return 1;
        }
        if arg.starts_with("-D") || arg.starts_with("-U") {
            return self.consume_macro_flag(args, i);
        }
        if matches!(arg.as_str(), "-nostdinc" | "-fno-pic" | "-fpic" | "-fPIC") {
            self.clang_flags.push(arg.clone());
        }
        1
    }

    fn consume_language_or_arch_flag(&mut self, args: &[String], i: usize) -> Option<usize> {
        let arg = &args[i];
        if let Some(lang) = arg.strip_prefix("-x") {
            let (lang_val, step) = inline_or_next(lang, args, i);
            match lang_val {
                Some("assembler-with-cpp") => self.lang_override = Some(true),
                Some("assembler") => self.lang_override = Some(false),
                _ => {}
            }
            return Some(step);
        }
        if let Some(val) = arg.strip_prefix("--target=") {
            self.target = Some(val.to_owned());
            self.clang_flags.push(arg.clone());
            return Some(1);
        }
        if (arg == "--target" || arg == "-target") && i + 1 < args.len() {
            let val = args[i + 1].clone();
            self.target = Some(val.clone());
            self.clang_flags.push(format!("--target={val}"));
            return Some(2);
        }
        if let Some(val) = arg.strip_prefix("-march=") {
            self.march = Some(val.to_owned());
            self.clang_flags.push(arg.clone());
            return Some(1);
        }
        if arg.starts_with("-mabi=") {
            self.clang_flags.push(arg.clone());
            return Some(1);
        }
        if arg == "-mabi" && i + 1 < args.len() {
            self.clang_flags.push(arg.clone());
            self.clang_flags.push(args[i + 1].clone());
            return Some(2);
        }
        None
    }

    fn consume_path_flag(&mut self, dir: &Path, args: &[String], i: usize) -> Option<usize> {
        let arg = &args[i];
        for (prefix, track_include, track_quote) in [
            ("-I", true, false),
            ("-iquote", false, true),
            ("-isystem", true, false),
            ("-include", false, false),
            ("-imacros", false, false),
            ("-isysroot", false, false),
        ] {
            if let Some(rest) = arg.strip_prefix(prefix) {
                let (raw_opt, step) = inline_or_next(rest, args, i);
                if let Some(raw) = raw_opt.filter(|s| !s.is_empty()) {
                    let path = resolve_path(dir, raw);
                    if track_include {
                        self.include_dirs.push(path.clone());
                    }
                    if track_quote {
                        self.quote_dirs.push(path.clone());
                    }
                    self.clang_flags.push(prefix.to_owned());
                    self.clang_flags.push(path.to_string_lossy().into_owned());
                }
                return Some(step);
            }
        }
        if let Some(d) = arg.strip_prefix("--sysroot=") {
            if !d.is_empty() {
                let path = resolve_path(dir, d);
                self.clang_flags
                    .push(format!("--sysroot={}", path.to_string_lossy()));
            }
            return Some(1);
        }
        if arg == "--sysroot" && i + 1 < args.len() {
            let path = resolve_path(dir, &args[i + 1]);
            self.clang_flags
                .push(format!("--sysroot={}", path.to_string_lossy()));
            return Some(2);
        }
        None
    }

    fn consume_wa_flags(&mut self, dir: &Path, wa: &str) {
        let wa_tokens: Vec<&str> = wa.split(',').collect();
        let mut wi = 0;
        let mut allowed_wa = Vec::new();
        while wi < wa_tokens.len() {
            let tok = wa_tokens[wi];
            if let Some(inc) = tok.strip_prefix("-I") {
                let (raw_opt, step) = inline_or_next_slice(inc, &wa_tokens, wi);
                wi += step - 1;
                if let Some(raw_path) = raw_opt.filter(|s| !s.is_empty()) {
                    let p = resolve_path(dir, raw_path);
                    self.include_dirs.push(p.clone());
                    allowed_wa.push(format!("-I{}", p.to_string_lossy()));
                }
            } else if let Some((flag, march_val, step)) =
                parse_wa_value(tok, "-march", &wa_tokens, wi)
            {
                wi += step - 1;
                if let Some(m) = march_val {
                    self.march = Some(m.to_owned());
                    allowed_wa.push(flag);
                }
            } else if let Some((flag, abi_val, step)) = parse_wa_value(tok, "-mabi", &wa_tokens, wi)
            {
                wi += step - 1;
                if abi_val.is_some() {
                    allowed_wa.push(flag);
                }
            }
            wi += 1;
        }
        if !allowed_wa.is_empty() {
            self.clang_flags
                .push(format!("-Wa,{}", allowed_wa.join(",")));
        }
    }

    fn consume_macro_flag(&mut self, args: &[String], i: usize) -> usize {
        let arg = &args[i];
        if (arg == "-D" || arg == "-U") && i + 1 < args.len() {
            self.clang_flags.push(arg.clone());
            self.clang_flags.push(args[i + 1].clone());
            2
        } else {
            self.clang_flags.push(arg.clone());
            1
        }
    }
}

fn inline_or_next<'a>(inline: &'a str, args: &'a [String], i: usize) -> (Option<&'a str>, usize) {
    if inline.is_empty() {
        if i + 1 < args.len() {
            (Some(args[i + 1].as_str()), 2)
        } else {
            (None, 1)
        }
    } else {
        (Some(inline), 1)
    }
}

fn inline_or_next_slice<'a>(
    inline: &'a str,
    tokens: &[&'a str],
    i: usize,
) -> (Option<&'a str>, usize) {
    if inline.is_empty() {
        if i + 1 < tokens.len() {
            (Some(tokens[i + 1]), 2)
        } else {
            (None, 1)
        }
    } else {
        (Some(inline), 1)
    }
}

fn parse_wa_value<'a>(
    tok: &'a str,
    name: &str,
    tokens: &[&'a str],
    wi: usize,
) -> Option<(String, Option<&'a str>, usize)> {
    if let Some(rest) = tok.strip_prefix(name)
        && let Some(val) = rest.strip_prefix('=')
    {
        return Some((format!("{name}={val}"), (!val.is_empty()).then_some(val), 1));
    }
    if tok == name && wi + 1 < tokens.len() {
        let val = tokens[wi + 1];
        return Some((format!("{name}={val}"), (!val.is_empty()).then_some(val), 2));
    }
    None
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
            push_double_quoted_char(ch, &mut chars, &mut current, &mut in_double);
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

fn push_double_quoted_char(
    ch: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    current: &mut String,
    in_double: &mut bool,
) {
    if ch == '"' {
        *in_double = false;
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
}
