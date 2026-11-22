mod parse;
mod scope;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use lsp_server::{Message, Notification, Request, RequestId};
use lsp_types::{
    Location, NumberOrString, ProgressParams, ProgressParamsValue, Range, Uri, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkDoneProgressReport,
};
use tree_sitter::Parser;

use crate::context::{normalize_path, path_to_uri, uri_to_path};
use crate::diagnostics::MessageSink;
use crate::document::Document;
use crate::position::LineIndex;

pub use scope::{
    enclosing_macro_at, parse_macro_var_token, parse_numeric_ref_token, resolve_numeric_def,
};

use parse::{build_file_index, is_eager_asm_path, is_header_path};

const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WORKSPACE_FILES: usize = 2_000;
const PROGRESS_TOKEN: &str = "riscv-asm-lsp/index";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    Label,
    LocalLabel,
    NumericLabel,
    Equ,
    Macro,
    MacroParam,
    Define,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Identifier,
    NumericForward,
    NumericBackward,
    MacroParam,
}

#[derive(Debug, Clone)]
pub struct MacroParamDef {
    pub name: String,
    pub span: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymKind,
    pub name_span: (usize, usize),
    pub full_span: (usize, usize),
    pub line_text: String,
    pub value_text: Option<String>,
    pub params: Vec<MacroParamDef>,
    pub enclosing_label_idx: Option<usize>,
    pub enclosing_macro_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub name: String,
    pub kind: RefKind,
    pub span: (usize, usize),
    pub enclosing_macro_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct IncludeDirective {
    pub target: String,
    pub is_system: bool,
}

#[derive(Debug, Clone)]
pub struct FileIndex {
    pub path: PathBuf,
    pub uri: Uri,
    pub is_header: bool,
    pub text: String,
    pub line_index: LineIndex,
    pub definitions: Vec<SymbolDef>,
    pub references: Vec<SymbolRef>,
    pub includes: Vec<IncludeDirective>,
    pub globals: HashSet<String>,
}

impl FileIndex {
    pub fn location(&self, span: (usize, usize)) -> Location {
        Location::new(
            self.uri.clone(),
            self.line_index
                .byte_span_to_range(&self.text, span.0, span.1),
        )
    }

    pub fn range(&self, span: (usize, usize)) -> Range {
        self.line_index
            .byte_span_to_range(&self.text, span.0, span.1)
    }
}

#[derive(Debug, Clone)]
pub struct CompletionSymbol {
    pub name: String,
    pub kind: SymKind,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VisibleCompletions {
    pub symbols: Vec<CompletionSymbol>,
    pub macros: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SymbolHoverInfo {
    pub name: String,
    pub kind: SymKind,
    pub def_line: String,
    pub value: Option<String>,
    pub target_uri: Option<Uri>,
    pub target_line_1based: Option<u32>,
    pub range: Range,
}

pub struct WorkspaceIndex {
    pub(super) workspace_root: Option<PathBuf>,
    pub(super) files: HashMap<PathBuf, FileIndex>,
    parser: Parser,
    bg_rx: Option<mpsc::Receiver<FileIndex>>,
    bg_cancel: Arc<AtomicBool>,
    bg_handle: Option<JoinHandle<()>>,
}

impl WorkspaceIndex {
    pub fn new(workspace_root: Option<PathBuf>) -> Result<Self, String> {
        let mut parser = Parser::new();
        let language = tree_sitter_riscv::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(|err| format!("failed to initialize tree-sitter-riscv: {err}"))?;
        Ok(Self {
            workspace_root,
            files: HashMap::new(),
            parser,
            bg_rx: None,
            bg_cancel: Arc::new(AtomicBool::new(false)),
            bg_handle: None,
        })
    }

    /// Spawn a background worker that indexes assembly files under `workspace_root`,
    /// skipping hidden and `.gitignore`d directories and stopping at `MAX_WORKSPACE_FILES`.
    pub fn start_background_scan(&mut self, progress_sink: Option<MessageSink>) {
        self.start_background_scan_with_cap(progress_sink, MAX_WORKSPACE_FILES);
    }

    fn start_background_scan_with_cap(&mut self, progress_sink: Option<MessageSink>, cap: usize) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        if self.bg_handle.is_some() {
            return;
        }

        if let Some(sink) = &progress_sink
            && let Ok(create_params) = serde_json::to_value(WorkDoneProgressCreateParams {
                token: NumberOrString::String(PROGRESS_TOKEN.to_owned()),
            })
        {
            sink(Message::Request(Request::new(
                RequestId::from(2),
                "window/workDoneProgress/create".to_owned(),
                create_params,
            )));
            send_progress(
                sink,
                WorkDoneProgress::Begin(WorkDoneProgressBegin {
                    title: "Indexing RISC-V assembly".to_owned(),
                    cancellable: Some(false),
                    message: Some("Scanning workspace files".to_owned()),
                    percentage: None,
                }),
            );
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.bg_cancel = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();
        self.bg_rx = Some(rx);

        let handle = thread::spawn(move || {
            let scanned =
                scan_directory_in_background(&root, cap, &cancel, &tx, progress_sink.as_ref());
            if let Some(sink) = &progress_sink {
                send_progress(
                    sink,
                    WorkDoneProgress::End(WorkDoneProgressEnd {
                        message: Some(format!("Indexed {scanned} files")),
                    }),
                );
            }
        });
        self.bg_handle = Some(handle);
    }

    /// Non-blocking drain of completed background file parses into the index.
    pub fn drain_background(&mut self, is_open: &impl Fn(&Uri) -> bool) {
        let Some(rx) = self.bg_rx.take() else {
            return;
        };
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(entry) => {
                    if !is_open(&entry.uri) {
                        self.files.entry(entry.path.clone()).or_insert(entry);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if !disconnected {
            self.bg_rx = Some(rx);
        }
    }

    /// Drain background scan results, waiting up to `budget` if scanning is still running.
    pub fn drain_background_wait(&mut self, is_open: &impl Fn(&Uri) -> bool, budget: Duration) {
        self.drain_background(is_open);
        let Some(rx) = self.bg_rx.take() else {
            return;
        };
        let deadline = Instant::now() + budget;
        let mut disconnected = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(entry) => {
                    if !is_open(&entry.uri) {
                        self.files.entry(entry.path.clone()).or_insert(entry);
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if !disconnected {
            self.bg_rx = Some(rx);
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_for_background_scan(&mut self) {
        if let Some(handle) = self.bg_handle.take() {
            let _ = handle.join();
        }
        self.drain_background(&|_| false);
    }

    pub fn shutdown(&mut self) {
        self.bg_cancel.store(true, Ordering::Relaxed);
        self.bg_rx.take();
        if let Some(handle) = self.bg_handle.take() {
            let _ = handle.join();
        }
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub fn files(&self) -> impl Iterator<Item = &FileIndex> {
        self.files.values()
    }

    pub fn file_by_uri(&self, uri: &Uri) -> Option<&FileIndex> {
        let path = uri_to_path(uri)?;
        let norm = normalize_path(&path);
        self.files.get(&norm).or_else(|| self.files.get(&path))
    }

    pub fn upsert_open_document(&mut self, doc: &Document) {
        let Some(path) = uri_to_path(&doc.uri) else {
            return;
        };
        let norm = normalize_path(&path);
        let is_header = is_header_path(&norm);
        let entry = build_file_index(
            norm.clone(),
            doc.uri.clone(),
            is_header,
            doc.text.clone(),
            doc.line_index.clone(),
            doc.tree.as_ref(),
            &mut self.parser,
        );
        self.files.insert(norm, entry);
    }

    pub fn reload_path(&mut self, uri: &Uri) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let norm = normalize_path(&path);
        if norm.exists() {
            self.index_path_from_disk(&norm);
        } else {
            self.files.remove(&norm);
        }
    }

    pub fn remove_path(&mut self, uri: &Uri) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let norm = normalize_path(&path);
        self.files.remove(&norm);
        self.files.remove(&path);
    }

    pub(super) fn index_path_from_disk(&mut self, path: &Path) {
        if let Some(entry) = parse_file_from_disk(path, &mut self.parser) {
            self.files.insert(entry.path.clone(), entry);
        }
    }
}

impl Drop for WorkspaceIndex {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn parse_file_from_disk(path: &Path, parser: &mut Parser) -> Option<FileIndex> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEX_FILE_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let norm = normalize_path(path);
    let uri = path_to_uri(&norm)?;
    let line_index = LineIndex::new(&text);
    let is_header = is_header_path(&norm);
    Some(build_file_index(
        norm, uri, is_header, text, line_index, None, parser,
    ))
}

fn scan_directory_in_background(
    root: &Path,
    cap: usize,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<FileIndex>,
    progress_sink: Option<&MessageSink>,
) -> usize {
    let mut parser = Parser::new();
    let language = tree_sitter_riscv::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return 0;
    }

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                return !matches!(name.as_ref(), "target" | "build" | "node_modules");
            }
            true
        });

    let mut scanned = 0usize;
    for entry in builder.build().flatten() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if !is_eager_asm_path(path) {
            continue;
        }
        if scanned >= cap {
            eprintln!("riscv-asm-lsp: workspace scan capped at {cap} files");
            break;
        }
        if let Some(file_idx) = parse_file_from_disk(path, &mut parser) {
            if tx.send(file_idx).is_err() {
                break;
            }
            scanned += 1;
            if let Some(sink) = progress_sink
                && scanned.is_multiple_of(50)
            {
                send_progress(
                    sink,
                    WorkDoneProgress::Report(WorkDoneProgressReport {
                        cancellable: Some(false),
                        message: Some(format!("Indexed {scanned} files")),
                        percentage: None,
                    }),
                );
            }
        }
    }
    scanned
}

fn send_progress(sink: &MessageSink, progress: WorkDoneProgress) {
    if let Ok(params) = serde_json::to_value(ProgressParams {
        token: NumberOrString::String(PROGRESS_TOKEN.to_owned()),
        value: ProgressParamsValue::WorkDone(progress),
    }) {
        sink(Message::Notification(Notification::new(
            "$/progress".to_owned(),
            params,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CompileContext;
    use crate::document::DocumentStore;
    use std::str::FromStr;

    #[test]
    fn indexes_kernel_entry_symbols_and_hierarchy() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("root")
            .join("tests/fixtures");
        let mut idx = WorkspaceIndex::new(Some(fixtures.clone())).expect("index");
        assert!(
            idx.files.is_empty(),
            "new() must not block on directory walk"
        );
        idx.start_background_scan(None);
        idx.wait_for_background_scan();

        // Headers are not indexed eagerly until `#include` closure reaches them
        let regs_path = normalize_path(&fixtures.join("regs.h"));
        assert!(!idx.files.contains_key(&regs_path));

        let mut ctx = CompileContext::new(Some(fixtures.clone()), Default::default());
        let kernel_path = normalize_path(&fixtures.join("kernel_entry.S"));
        let kernel_uri = path_to_uri(&kernel_path).expect("uri");
        let file = idx
            .file_by_uri(&kernel_uri)
            .expect("kernel indexed")
            .clone();

        assert!(
            file.definitions
                .iter()
                .any(|d| d.kind == SymKind::Equ && d.name == "KERNEL_BASE")
        );
        assert!(
            file.definitions
                .iter()
                .any(|d| d.kind == SymKind::Macro && d.name == "SAVE_REG")
        );
        let start_label = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::Label && d.name == "_start")
            .expect("_start label");
        let trap_label = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::Label && d.name == "trap_vector")
            .expect("trap_vector label");
        assert!(start_label.full_span.1 < trap_label.name_span.0);

        let num_1 = file
            .definitions
            .iter()
            .find(|d| d.kind == SymKind::NumericLabel && d.name == "1")
            .expect("numeric label 1");
        assert!(num_1.enclosing_label_idx.is_some());

        // Header #define resolved lazily through `#include "regs.h"`
        let closure = idx.resolve_include_closure(&kernel_path, &mut ctx);
        assert!(closure.contains(&regs_path));
        let regs_file = idx.files.get(&regs_path).expect("regs.h indexed");
        assert!(
            regs_file
                .definitions
                .iter()
                .any(|d| d.kind == SymKind::Define && d.name == "MSTATUS_MIE")
        );
    }

    #[test]
    fn background_scan_skips_hidden_gitignored_headers_and_respects_cap() {
        let tmp = std::env::temp_dir().join(format!("riscv_asm_lsp_walk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".hidden")).unwrap();
        fs::create_dir_all(tmp.join("ignored_dir")).unwrap();
        fs::write(tmp.join(".gitignore"), "ignored_dir/\n").unwrap();
        fs::write(tmp.join(".hidden/secret.S"), "hidden_sym:\n    nop\n").unwrap();
        fs::write(tmp.join("ignored_dir/skip.S"), "gitignored_sym:\n    nop\n").unwrap();
        fs::write(tmp.join("header_only.h"), "#define HDR_ONLY 1\n").unwrap();
        fs::write(tmp.join("a.S"), "sym_a:\n    nop\n").unwrap();
        fs::write(tmp.join("b.S"), "sym_b:\n    nop\n").unwrap();
        fs::write(tmp.join("c.S"), "sym_c:\n    nop\n").unwrap();

        let mut idx = WorkspaceIndex::new(Some(tmp.clone())).unwrap();
        idx.start_background_scan_with_cap(None, 2);
        idx.wait_for_background_scan();

        assert!(!idx.files.keys().any(|p| p.ends_with("secret.S")
            || p.ends_with("skip.S")
            || p.ends_with("header_only.h")));
        assert_eq!(idx.files.len(), 2);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn indexes_define_directives_in_assembly_files() {
        let mut store = DocumentStore::new().unwrap();
        let uri = Uri::from_str("file:///zed_check.S").unwrap();
        let doc = store.open(
            uri.clone(),
            1,
            "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n".to_owned(),
        );
        let mut ctx = CompileContext::new(None, Default::default());
        let mut idx = WorkspaceIndex::new(None).unwrap();
        idx.upsert_open_document(doc);

        let consts = idx.constant_defines_in_scope(&uri, &mut ctx);
        assert_eq!(consts.get("LOCAL_VALUE").map(String::as_str), Some("42"));
        let hover = idx
            .token_hover_info(&uri, 46, &mut ctx)
            .expect("LOCAL_VALUE hover");
        assert_eq!(hover.kind, SymKind::Define);
        assert_eq!(hover.value.as_deref(), Some("42"));
    }
}
