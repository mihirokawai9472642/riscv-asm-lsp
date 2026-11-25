mod parse;
mod scope;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
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

use crate::context::{
    display_path_for_client, is_linker_script_template, is_preprocessed_by_suffix, path_key,
    path_starts_with, path_to_uri, uri_to_path,
};
use crate::diagnostics::MessageSink;
use crate::document::Document;
use crate::position::LineIndex;

pub use scope::{
    IncludeKind, enclosing_macro_at, parse_macro_var_token, parse_numeric_ref_token,
    resolve_numeric_def, resolve_param_scope, visible_params_at,
};

use parse::{BuildFileInput, build_file_index, is_eager_asm_path, is_header_path};

const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WORKSPACE_FILES: usize = 2_000;
const FOREIGN_PROBE_BYTES: usize = 4096;
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
pub enum AssignKind {
    Equ,
    Equiv,
    Set,
    Equals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamScope {
    Macro(usize),
    Repeat(usize),
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
pub struct RepeatScope {
    pub directive: String,
    pub param: MacroParamDef,
    pub full_span: (usize, usize),
    pub line_text: String,
}

#[derive(Debug, Clone)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymKind,
    pub assign_kind: Option<AssignKind>,
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
    pub preprocessed: bool,
    pub text: String,
    pub line_index: LineIndex,
    pub definitions: Vec<SymbolDef>,
    pub repeat_scopes: Vec<RepeatScope>,
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
    pub assign_kind: Option<AssignKind>,
    pub assignment_count: usize,
    pub param_label: Option<String>,
    pub def_line: String,
    pub value: Option<String>,
    pub target_uri: Option<Uri>,
    pub target_line_1based: Option<u32>,
    pub range: Range,
}

pub struct WorkspaceIndex {
    pub(super) workspace_root: Option<PathBuf>,
    pub(super) canon_workspace_root: Option<PathBuf>,
    workspace_roots: Vec<PathBuf>,
    canon_workspace_roots: Vec<PathBuf>,
    pub(super) files: HashMap<PathBuf, FileIndex>,
    parser: Parser,
    seen_paths: Arc<Mutex<HashSet<PathBuf>>>,
    excluded_paths: Arc<Mutex<HashSet<PathBuf>>>,
    priority_rxs: Vec<mpsc::Receiver<FileIndex>>,
    priority_handles: Vec<JoinHandle<()>>,
    prioritized_roots: HashSet<PathBuf>,
    bg_rx: Option<mpsc::Receiver<FileIndex>>,
    bg_cancel: Arc<AtomicBool>,
    bg_handle: Option<JoinHandle<()>>,
}

impl WorkspaceIndex {
    pub fn new(workspace_root: Option<PathBuf>) -> Result<Self, String> {
        Self::new_multi(workspace_root.into_iter().collect())
    }

    pub fn new_multi(workspace_roots: Vec<PathBuf>) -> Result<Self, String> {
        let mut parser = Parser::new();
        let language = tree_sitter_riscv::LANGUAGE.into();
        parser
            .set_language(&language)
            .map_err(|err| format!("failed to initialize tree-sitter-riscv: {err}"))?;
        let canon_workspace_roots: Vec<PathBuf> =
            workspace_roots.iter().map(|r| path_key(r)).collect();
        let workspace_root = workspace_roots.first().cloned();
        let canon_workspace_root = canon_workspace_roots.first().cloned();
        Ok(Self {
            workspace_root,
            canon_workspace_root,
            workspace_roots,
            canon_workspace_roots,
            files: HashMap::new(),
            parser,
            seen_paths: Arc::new(Mutex::new(HashSet::new())),
            excluded_paths: Arc::new(Mutex::new(HashSet::new())),
            priority_rxs: Vec::new(),
            priority_handles: Vec::new(),
            prioritized_roots: HashSet::new(),
            bg_rx: None,
            bg_cancel: Arc::new(AtomicBool::new(false)),
            bg_handle: None,
        })
    }

    pub fn set_excluded_paths(&mut self, paths: HashSet<PathBuf>) {
        let keyed: HashSet<PathBuf> = paths.into_iter().map(|p| path_key(&p)).collect();
        for p in &keyed {
            self.files.remove(p);
        }
        if let Ok(mut guard) = self.excluded_paths.lock() {
            *guard = keyed;
        }
    }

    fn is_excluded_path(&self, norm_path: &Path) -> bool {
        let key = path_key(norm_path);
        self.excluded_paths
            .lock()
            .map(|g| g.contains(&key))
            .unwrap_or(false)
    }

    pub fn display_path(&self, path: &Path) -> PathBuf {
        let key = path_key(path);
        for (client_root, canon_root) in self
            .workspace_roots
            .iter()
            .zip(self.canon_workspace_roots.iter())
        {
            if path_starts_with(&key, canon_root) {
                return display_path_for_client(path, Some(client_root), Some(canon_root));
            }
        }
        display_path_for_client(
            path,
            self.workspace_root.as_deref(),
            self.canon_workspace_root.as_deref(),
        )
    }

    /// Spawn a background worker that indexes assembly files under `workspace_root`,
    /// skipping hidden, `.gitignore`d, and foreign-architecture files and stopping at
    /// `MAX_WORKSPACE_FILES`.
    pub fn start_background_scan(&mut self, progress_sink: Option<MessageSink>) {
        self.start_background_scan_with_id(progress_sink, RequestId::from(2));
    }

    pub fn start_background_scan_with_id(
        &mut self,
        progress_sink: Option<MessageSink>,
        progress_req_id: RequestId,
    ) {
        self.start_background_scan_with_cap(progress_sink, MAX_WORKSPACE_FILES, progress_req_id);
    }

    /// Update indexed workspace roots after `workspace/didChangeWorkspaceFolders`,
    /// pruning non-open files from removed roots and scanning newly added roots.
    pub fn update_workspace_roots(
        &mut self,
        workspace_roots: Vec<PathBuf>,
        is_open_path: &impl Fn(&Path) -> bool,
        progress_sink: Option<MessageSink>,
        progress_req_id: RequestId,
    ) {
        self.bg_cancel.store(true, Ordering::Relaxed);
        self.bg_rx.take();
        if let Some(handle) = self.bg_handle.take() {
            let _ = handle.join();
        }
        self.bg_cancel = Arc::new(AtomicBool::new(false));

        let canon_roots: Vec<PathBuf> = workspace_roots.iter().map(|r| path_key(r)).collect();
        self.files.retain(|path, _| {
            is_open_path(path) || canon_roots.iter().any(|r| path_starts_with(path, r))
        });
        if let Ok(mut seen) = self.seen_paths.lock() {
            seen.retain(|p| self.files.contains_key(p));
        }
        self.prioritized_roots.retain(|r| canon_roots.contains(r));

        self.workspace_root = workspace_roots.first().cloned();
        self.canon_workspace_root = canon_roots.first().cloned();
        self.workspace_roots = workspace_roots;
        self.canon_workspace_roots = canon_roots;

        self.start_background_scan_with_id(progress_sink, progress_req_id);
    }

    fn start_background_scan_with_cap(
        &mut self,
        progress_sink: Option<MessageSink>,
        cap: usize,
        progress_req_id: RequestId,
    ) {
        if self.workspace_roots.is_empty() || self.bg_handle.is_some() {
            return;
        }

        if let Some(sink) = &progress_sink
            && let Ok(create_params) = serde_json::to_value(WorkDoneProgressCreateParams {
                token: NumberOrString::String(PROGRESS_TOKEN.to_owned()),
            })
        {
            sink(Message::Request(Request::new(
                progress_req_id,
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

        let cancel = Arc::clone(&self.bg_cancel);
        let seen = Arc::clone(&self.seen_paths);
        let excluded = Arc::clone(&self.excluded_paths);
        let roots: Vec<(PathBuf, PathBuf)> = self
            .workspace_roots
            .iter()
            .cloned()
            .zip(self.canon_workspace_roots.iter().cloned())
            .collect();
        let (tx, rx) = mpsc::channel();
        self.bg_rx = Some(rx);

        let handle = thread::spawn(move || {
            let mut logged_panic = false;
            let mut total_scanned = 0usize;
            for (root, canon_root) in &roots {
                if total_scanned >= cap || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let remaining_cap = cap - total_scanned;
                let scanned = run_guarded_index_step("background scan", &mut logged_panic, || {
                    scan_directory_in_background(ScanTask {
                        root,
                        client_root: Some(root),
                        canon_root: Some(canon_root),
                        cap: remaining_cap,
                        cancel: &cancel,
                        seen: &seen,
                        excluded: &excluded,
                        tx: &tx,
                        progress_sink: progress_sink.as_ref(),
                    })
                })
                .unwrap_or(0);
                total_scanned += scanned;
            }
            if let Some(sink) = &progress_sink {
                send_progress(
                    sink,
                    WorkDoneProgress::End(WorkDoneProgressEnd {
                        message: Some(format!("Indexed {total_scanned} files")),
                    }),
                );
            }
        });
        self.bg_handle = Some(handle);
    }

    /// Prioritize scanning the project subtree owning a newly opened document so
    /// cross-file lookups work rapidly even when `workspace_root` is a broad directory
    /// such as `$HOME`.
    pub fn prioritize_open_document(&mut self, uri: &Uri) {
        self.reap_finished_priority_handles();
        let Some(file_path) = uri_to_path(uri) else {
            return;
        };
        if is_linker_script_template(&file_path) {
            return;
        }
        let Some(scan_root) = find_priority_root(
            &file_path,
            self.workspace_root.as_deref(),
            self.canon_workspace_root.as_deref(),
        ) else {
            return;
        };
        let norm_root = path_key(&scan_root);
        if !self.prioritized_roots.insert(norm_root.clone()) {
            return;
        }
        let display_root = self.display_path(&scan_root);
        let client_root = self.workspace_root.clone();
        let canon_root = self.canon_workspace_root.clone();
        let cancel = Arc::clone(&self.bg_cancel);
        let seen = Arc::clone(&self.seen_paths);
        let excluded = Arc::clone(&self.excluded_paths);
        let (tx, rx) = mpsc::channel();
        self.priority_rxs.push(rx);

        let handle = thread::spawn(move || {
            let mut logged_panic = false;
            let _ = run_guarded_index_step("priority scan", &mut logged_panic, || {
                scan_directory_in_background(ScanTask {
                    root: &display_root,
                    client_root: client_root.as_deref(),
                    canon_root: canon_root.as_deref(),
                    cap: MAX_WORKSPACE_FILES,
                    cancel: &cancel,
                    seen: &seen,
                    excluded: &excluded,
                    tx: &tx,
                    progress_sink: None,
                });
            });
        });
        self.priority_handles.push(handle);
    }

    fn reap_finished_priority_handles(&mut self) {
        let mut active = Vec::with_capacity(self.priority_handles.len());
        for h in self.priority_handles.drain(..) {
            if h.is_finished() {
                let _ = h.join();
            } else {
                active.push(h);
            }
        }
        self.priority_handles = active;
    }

    /// Non-blocking drain of completed priority and background file parses into the index.
    pub fn drain_background(&mut self, is_open_path: &impl Fn(&Path) -> bool) {
        drain_receiver_vec(&mut self.priority_rxs, &mut self.files, is_open_path);
        self.reap_finished_priority_handles();
        let Some(rx) = self.bg_rx.take() else {
            return;
        };
        let disconnected = drain_single_rx_nonblocking(&rx, &mut self.files, is_open_path);
        if !disconnected {
            self.bg_rx = Some(rx);
        }
    }

    /// Drain background scan results, prioritizing open-document project scans and
    /// waiting up to `budget` if scanning is still running.
    pub fn drain_background_wait(
        &mut self,
        is_open_path: &impl Fn(&Path) -> bool,
        budget: Duration,
    ) {
        self.drain_background(is_open_path);
        let deadline = Instant::now() + budget;

        // First finish any active priority project scans within budget.
        let mut remaining_priority = Vec::new();
        for rx in self.priority_rxs.drain(..) {
            let disconnected = drain_single_rx_until(&rx, &mut self.files, is_open_path, deadline);
            if !disconnected {
                remaining_priority.push(rx);
            }
        }
        self.priority_rxs = remaining_priority;

        let Some(rx) = self.bg_rx.take() else {
            return;
        };
        let disconnected = drain_single_rx_until(&rx, &mut self.files, is_open_path, deadline);
        if !disconnected {
            self.bg_rx = Some(rx);
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_for_background_scan(&mut self) {
        for h in self.priority_handles.drain(..) {
            let _ = h.join();
        }
        if let Some(handle) = self.bg_handle.take() {
            let _ = handle.join();
        }
        self.drain_background(&|_| false);
    }

    pub fn shutdown(&mut self) {
        self.bg_cancel.store(true, Ordering::Relaxed);
        self.priority_rxs.clear();
        self.bg_rx.take();
        for h in self.priority_handles.drain(..) {
            let _ = h.join();
        }
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

    pub fn file_by_path(&self, path: &Path) -> Option<&FileIndex> {
        self.files
            .get(path)
            .or_else(|| self.files.get(&path_key(path)))
    }

    pub fn file_by_uri(&self, uri: &Uri) -> Option<&FileIndex> {
        let path = uri_to_path(uri)?;
        let key = path_key(&path);
        self.files.get(&key).or_else(|| self.files.get(&path))
    }

    pub fn upsert_open_document(&mut self, doc: &Document) {
        let Some(path) = uri_to_path(&doc.uri) else {
            return;
        };
        let key = path_key(&path);
        let preprocessed = is_preprocessed_by_suffix(&key);
        self.upsert_open_document_with_preprocessed(doc, preprocessed);
    }

    pub fn upsert_open_document_with_preprocessed(&mut self, doc: &Document, preprocessed: bool) {
        let Some(path) = uri_to_path(&doc.uri) else {
            return;
        };
        let key = path_key(&path);
        if is_linker_script_template(&key) {
            return;
        }
        let is_header = is_header_path(&key);
        let effective_pre = if is_header { true } else { preprocessed };
        let entry = build_file_index(
            BuildFileInput {
                path: key.clone(),
                uri: doc.uri.clone(),
                is_header,
                preprocessed: effective_pre,
                text: doc.text.clone(),
                line_index: doc.line_index.clone(),
                existing_tree: doc.tree.as_ref(),
            },
            &mut self.parser,
        );
        self.files.insert(key, entry);
    }

    pub fn sync_preprocessed(&mut self, uri: &Uri, preprocessed: bool) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let key = path_key(&path);
        self.sync_preprocessed_path(&key, preprocessed);
    }

    pub(super) fn sync_preprocessed_path(&mut self, path: &Path, preprocessed: bool) {
        let key = path_key(path);
        let Some(existing) = self.files.get(&key) else {
            return;
        };
        if existing.is_header || existing.preprocessed == preprocessed {
            return;
        }
        let uri = existing.uri.clone();
        let text = existing.text.clone();
        let line_index = existing.line_index.clone();
        let rebuilt = build_file_index(
            BuildFileInput {
                path: key.clone(),
                uri,
                is_header: false,
                preprocessed,
                text,
                line_index,
                existing_tree: None,
            },
            &mut self.parser,
        );
        self.files.insert(key, rebuilt);
    }

    pub fn reload_path(&mut self, uri: &Uri) {
        self.reload_path_with_preprocessed(uri, None);
    }

    pub fn reload_path_with_preprocessed(
        &mut self,
        uri: &Uri,
        preprocessed_override: Option<bool>,
    ) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let key = path_key(&path);
        if is_linker_script_template(&key) || self.is_excluded_path(&key) {
            self.files.remove(&key);
            return;
        }
        if path.exists() {
            let display = self.display_path(&path);
            self.index_path_from_disk_with_display(&display, &key, preprocessed_override);
        } else {
            self.files.remove(&key);
        }
    }

    pub fn remove_path(&mut self, uri: &Uri) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let key = path_key(&path);
        self.files.remove(&key);
        self.files.remove(&path);
    }

    pub(super) fn index_path_from_disk_with_preprocessed(
        &mut self,
        path: &Path,
        preprocessed_override: Option<bool>,
    ) {
        let key = path_key(path);
        let display = self.display_path(path);
        self.index_path_from_disk_with_display(&display, &key, preprocessed_override);
    }

    fn index_path_from_disk_with_display(
        &mut self,
        display_path: &Path,
        norm_path: &Path,
        preprocessed_override: Option<bool>,
    ) {
        if let Some(entry) = parse_file_from_disk(
            DiskParseRequest {
                path: display_path,
                known_norm: Some(norm_path),
                client_root: self.workspace_root.as_deref(),
                canon_root: self.canon_workspace_root.as_deref(),
                preprocessed_override,
                seen: None,
                excluded: None,
            },
            &mut self.parser,
        ) {
            self.files.insert(entry.path.clone(), entry);
        }
    }
}

impl Drop for WorkspaceIndex {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn drain_receiver_vec(
    rxs: &mut Vec<mpsc::Receiver<FileIndex>>,
    files: &mut HashMap<PathBuf, FileIndex>,
    is_open_path: &impl Fn(&Path) -> bool,
) {
    let mut keep = Vec::with_capacity(rxs.len());
    for rx in rxs.drain(..) {
        if !drain_single_rx_nonblocking(&rx, files, is_open_path) {
            keep.push(rx);
        }
    }
    *rxs = keep;
}

fn drain_single_rx_nonblocking(
    rx: &mpsc::Receiver<FileIndex>,
    files: &mut HashMap<PathBuf, FileIndex>,
    is_open_path: &impl Fn(&Path) -> bool,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(entry) => {
                if !is_open_path(&entry.path) {
                    files.entry(entry.path.clone()).or_insert(entry);
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

fn drain_single_rx_until(
    rx: &mpsc::Receiver<FileIndex>,
    files: &mut HashMap<PathBuf, FileIndex>,
    is_open_path: &impl Fn(&Path) -> bool,
    deadline: Instant,
) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(entry) => {
                if !is_open_path(&entry.path) {
                    files.entry(entry.path.clone()).or_insert(entry);
                }
            }
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

fn find_priority_root(
    file_path: &Path,
    client_root: Option<&Path>,
    canon_root: Option<&Path>,
) -> Option<PathBuf> {
    let parent = file_path.parent()?;
    if !parent.is_dir() {
        return None;
    }
    let norm_parent = crate::context::normalize_path(parent);
    let key_parent = path_key(parent);
    if canon_root.is_some_and(|ws| ws == key_parent)
        || client_root.is_some_and(|ws| path_key(ws) == key_parent)
    {
        return None;
    }

    let mut cur = Some(norm_parent.as_path());
    while let Some(dir) = cur {
        if canon_root.is_some_and(|ws| ws == path_key(dir)) {
            return Some(parent.to_path_buf());
        }
        if dir.join(".git").exists() || dir.join("compile_commands.json").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    Some(parent.to_path_buf())
}

struct DiskParseRequest<'a> {
    path: &'a Path,
    known_norm: Option<&'a Path>,
    client_root: Option<&'a Path>,
    canon_root: Option<&'a Path>,
    preprocessed_override: Option<bool>,
    seen: Option<&'a Mutex<HashSet<PathBuf>>>,
    excluded: Option<&'a Mutex<HashSet<PathBuf>>>,
}

fn parse_file_from_disk(req: DiskParseRequest<'_>, parser: &mut Parser) -> Option<FileIndex> {
    if is_linker_script_template(req.path) {
        return None;
    }
    let key = req
        .known_norm
        .map(path_key)
        .unwrap_or_else(|| path_key(req.path));
    if let Some(excluded_lock) = req.excluded {
        let guard = excluded_lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains(&key) {
            return None;
        }
    }
    let mut file = fs::File::open(req.path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEX_FILE_BYTES {
        return None;
    }

    // Read only a small prefix first to reject binary / foreign assembly (such as Go Plan 9 `.s`)
    // before spending time allocating or invoking tree-sitter error recovery.
    let mut buf = Vec::with_capacity((meta.len() as usize).min(FOREIGN_PROBE_BYTES));
    (&mut file)
        .take(FOREIGN_PROBE_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if is_foreign_assembly(&buf) {
        return None;
    }

    if let Some(seen_lock) = req.seen {
        let mut guard = seen_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.insert(key.clone()) {
            return None;
        }
    }

    if (meta.len() as usize) > buf.len() {
        buf.reserve((meta.len() as usize).saturating_sub(buf.len()));
        file.read_to_end(&mut buf).ok()?;
    }
    let text = String::from_utf8(buf).ok()?;
    let display = display_path_for_client(req.path, req.client_root, req.canon_root);
    let uri = path_to_uri(&display)?;
    let line_index = LineIndex::new(&text);
    let is_header = is_header_path(&key);
    let preprocessed = req
        .preprocessed_override
        .unwrap_or_else(|| is_preprocessed_by_suffix(&key));
    Some(build_file_index(
        BuildFileInput {
            path: key,
            uri,
            is_header,
            preprocessed,
            text,
            line_index,
            existing_tree: None,
        },
        parser,
    ))
}

fn is_foreign_assembly(head: &[u8]) -> bool {
    if head.contains(&0) {
        return true;
    }
    let s = String::from_utf8_lossy(head);
    for line in s.lines() {
        let t = line.trim();
        if t.contains("#include \"textflag.h\"")
            || t.contains("#include \"go_asm.h\"")
            || t.contains("#include \"funcdata.h\"")
        {
            return true;
        }
        let is_go_decl = t.starts_with("TEXT ")
            || t.starts_with("TEXT\t")
            || t.starts_with("DATA ")
            || t.starts_with("DATA\t")
            || t.starts_with("GLOBL ")
            || t.starts_with("GLOBL\t");
        if is_go_decl && t.contains("(SB)") {
            return true;
        }
    }
    false
}

struct ScanTask<'a> {
    root: &'a Path,
    client_root: Option<&'a Path>,
    canon_root: Option<&'a Path>,
    cap: usize,
    cancel: &'a AtomicBool,
    seen: &'a Mutex<HashSet<PathBuf>>,
    excluded: &'a Mutex<HashSet<PathBuf>>,
    tx: &'a mpsc::Sender<FileIndex>,
    progress_sink: Option<&'a MessageSink>,
}

fn scan_directory_in_background(task: ScanTask<'_>) -> usize {
    let mut parser = Parser::new();
    let language = tree_sitter_riscv::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return 0;
    }

    let mut builder = WalkBuilder::new(task.root);
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
                if matches!(name.as_ref(), "target" | "node_modules") {
                    return false;
                }
                if name.as_ref() == "build" {
                    let p = entry.path();
                    return !(p.join("CMakeCache.txt").exists()
                        || p.join("build.ninja").exists()
                        || p.join("Makefile").exists()
                        || p.join("meson-info").exists());
                }
            }
            true
        });

    let mut scanned = 0usize;
    let mut logged_panic = false;
    for entry in builder.build().flatten() {
        if task.cancel.load(Ordering::Relaxed) {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if !is_eager_asm_path(path) {
            continue;
        }
        if scanned >= task.cap {
            eprintln!("riscv-asm-lsp: workspace scan capped at {} files", task.cap);
            break;
        }
        let parsed = run_guarded_index_step("parse file", &mut logged_panic, || {
            parse_file_from_disk(
                DiskParseRequest {
                    path,
                    known_norm: None,
                    client_root: task.client_root,
                    canon_root: task.canon_root,
                    preprocessed_override: None,
                    seen: Some(task.seen),
                    excluded: Some(task.excluded),
                },
                &mut parser,
            )
        })
        .flatten();
        if let Some(file_idx) = parsed {
            if task.tx.send(file_idx).is_err() {
                break;
            }
            scanned += 1;
            if let Some(sink) = task.progress_sink
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

fn run_guarded_index_step<T, F>(context: &str, logged_panic: &mut bool, f: F) -> Option<T>
where
    F: FnOnce() -> T,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(val) => Some(val),
        Err(payload) => {
            if !*logged_panic {
                *logged_panic = true;
                let msg = crate::server::panic_payload_message(&*payload);
                eprintln!("riscv-asm-lsp: panic in index worker ({context}): {msg}");
            }
            None
        }
    }
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
        let regs_path = path_key(&fixtures.join("regs.h"));
        assert!(!idx.files.contains_key(&regs_path));

        let mut ctx = CompileContext::new(Some(fixtures.clone()), Default::default());
        let kernel_path = crate::context::normalize_path(&fixtures.join("kernel_entry.S"));
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
        idx.start_background_scan_with_cap(None, 2, RequestId::from(2));
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
        let uri = crate::context::test_uri("zed_check.S");
        let doc = store.open(
            uri.clone(),
            1,
            "#define LOCAL_VALUE 42\n_start:\n    li t0, LOCAL_VALUE\n".to_owned(),
        );
        let mut ctx = CompileContext::new(None, Default::default());
        let mut idx = WorkspaceIndex::new(None).unwrap();
        idx.upsert_open_document(doc);

        let consts = idx.constant_defines_in_scope(&uri, None, &mut ctx);
        assert_eq!(consts.get("LOCAL_VALUE").map(String::as_str), Some("42"));
        let hover = idx
            .token_hover_info(&uri, 46, &mut ctx)
            .expect("LOCAL_VALUE hover");
        assert_eq!(hover.kind, SymKind::Define);
        assert_eq!(hover.value.as_deref(), Some("42"));
    }

    #[test]
    fn skips_go_plan9_assembly_and_prioritizes_open_project() {
        let tmp =
            std::env::temp_dir().join(format!("riscv_asm_lsp_go_prio_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let go_dir = tmp.join("go_pkg");
        let rv_dir = tmp.join("rv_proj");
        fs::create_dir_all(&go_dir).unwrap();
        fs::create_dir_all(rv_dir.join(".git")).unwrap();

        fs::write(
            go_dir.join("plan9_foo.s"),
            "#include \"textflag.h\"\nTEXT \u{00b7}foo(SB), NOSPLIT, $0-0\n    RET\n",
        )
        .unwrap();
        fs::write(rv_dir.join("a.S"), "_start:\n    call helper_fn\n").unwrap();
        fs::write(rv_dir.join("b.S"), "helper_fn:\n    ret\n").unwrap();

        let mut idx = WorkspaceIndex::new(Some(tmp.clone())).unwrap();
        idx.start_background_scan(None);

        let a_uri = path_to_uri(&rv_dir.join("a.S")).unwrap();
        idx.prioritize_open_document(&a_uri);
        idx.wait_for_background_scan();

        assert!(
            idx.files()
                .any(|f| f.definitions.iter().any(|d| d.name == "helper_fn"))
        );
        assert!(!idx.files().any(|f| f.path.ends_with("plan9_foo.s")));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn indexes_irp_irpc_scopes_and_reassigned_set_symbols() {
        let mut store = DocumentStore::new().unwrap();
        let uri = crate::context::test_uri("irp_set.S");
        let src = "\
.set CNT, 1
    li t0, CNT
.set CNT, CNT + 1
    li t1, CNT
.irp r, a0, a1
    mv \\r, zero
.endr
";
        let doc = store.open(uri.clone(), 1, src.to_owned());
        let mut ctx = CompileContext::new(None, Default::default());
        let mut idx = WorkspaceIndex::new(None).unwrap();
        idx.upsert_open_document(doc);

        let before_off = src.find("li t0, CNT").unwrap() + 8;
        let h_before = idx.token_hover_info(&uri, before_off, &mut ctx).unwrap();
        assert_eq!(h_before.value.as_deref(), Some("1"));
        assert_eq!(h_before.assign_kind, Some(AssignKind::Set));
        assert_eq!(h_before.assignment_count, 2);

        let after_off = src.find("li t1, CNT").unwrap() + 8;
        let h_after = idx.token_hover_info(&uri, after_off, &mut ctx).unwrap();
        assert_eq!(h_after.value.as_deref(), Some("CNT + 1"));
        assert_eq!(h_after.assignment_count, 2);

        let irp_use_off = src.find("\\r").unwrap() + 1;
        let h_irp = idx.token_hover_info(&uri, irp_use_off, &mut ctx).unwrap();
        assert_eq!(h_irp.name, "r");
        assert_eq!(h_irp.param_label.as_deref(), Some(".irp parameter"));
    }

    #[test]
    fn index_worker_guard_catches_panic_and_keeps_running() {
        let mut logged = false;
        let first: Option<usize> = run_guarded_index_step("test", &mut logged, || {
            panic!("simulated index panic");
        });
        assert!(first.is_none());
        assert!(logged);

        let second = run_guarded_index_step("test", &mut logged, || 42usize);
        assert_eq!(second, Some(42));
    }
}
