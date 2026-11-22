use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Position,
    PublishDiagnosticsParams, Range, Uri,
};

use crate::context::{FileContext, display_path_for_client, path_to_uri, uri_to_path};
use crate::position::{LineIndex, clamp_to_char_boundary, word_range_at};
use crate::preprocessor::{classify_hash_line, parse_cpp_define_line, parse_cpp_include_line};
use crate::tools::ToolRunner;

pub type MessageSink = Arc<dyn Fn(Message) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDefHint {
    pub macro_name: String,
    pub def_line: u32,
}

#[derive(Debug, Clone)]
pub struct StoredDiagnostic {
    pub diagnostic: Diagnostic,
    pub suggestions: Vec<String>,
    pub macro_def_hints: Vec<MacroDefHint>,
}

pub type DiagnosticsCache = Arc<Mutex<HashMap<String, Vec<StoredDiagnostic>>>>;

enum WorkerMsg {
    Check {
        uri: Uri,
        version: i32,
        text: String,
        file_ctx: Box<FileContext>,
    },
    Clear(Uri),
    Shutdown,
}

pub struct DiagnosticsWorker {
    tx: Option<mpsc::Sender<WorkerMsg>>,
    handle: Option<JoinHandle<()>>,
    latest_versions: Arc<Mutex<HashMap<String, i32>>>,
    pub cache: DiagnosticsCache,
}

impl DiagnosticsWorker {
    pub fn new(
        enable: bool,
        delay_ms: u64,
        tools: Arc<ToolRunner>,
        lsp_sender: MessageSink,
    ) -> Self {
        let latest_versions: Arc<Mutex<HashMap<String, i32>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cache: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

        if !enable {
            return Self {
                tx: None,
                handle: None,
                latest_versions,
                cache,
            };
        }

        let (tx, rx) = mpsc::channel();
        let versions_bg = Arc::clone(&latest_versions);
        let cache_bg = Arc::clone(&cache);
        let delay = Duration::from_millis(delay_ms);

        let handle = thread::spawn(move || {
            worker_loop(rx, delay, tools, lsp_sender, versions_bg, cache_bg);
        });

        Self {
            tx: Some(tx),
            handle: Some(handle),
            latest_versions,
            cache,
        }
    }

    pub fn schedule(&self, uri: Uri, version: i32, text: String, file_ctx: FileContext) {
        if let Ok(mut map) = self.latest_versions.lock() {
            map.insert(uri.as_str().to_owned(), version);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Check {
                uri,
                version,
                text,
                file_ctx: Box::new(file_ctx),
            });
        }
    }

    pub fn clear(&self, uri: &Uri) {
        let key = uri.as_str().to_owned();
        if let Ok(mut map) = self.latest_versions.lock() {
            map.remove(&key);
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(&key);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Clear(uri.clone()));
        }
    }

    pub fn shutdown(mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(WorkerMsg::Shutdown);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct PendingJob {
    uri: Uri,
    version: i32,
    text: String,
    file_ctx: FileContext,
    due_at: Instant,
}

fn worker_loop(
    rx: mpsc::Receiver<WorkerMsg>,
    delay: Duration,
    tools: Arc<ToolRunner>,
    lsp_sender: MessageSink,
    latest_versions: Arc<Mutex<HashMap<String, i32>>>,
    cache: DiagnosticsCache,
) {
    let mut pending: HashMap<String, PendingJob> = HashMap::new();
    let mut logged_panic = false;

    loop {
        let msg = if let Some(next_due) = pending.values().map(|j| j.due_at).min() {
            let now = Instant::now();
            if next_due <= now {
                None
            } else {
                match rx.recv_timeout(next_due.saturating_duration_since(now)) {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };

        if let Some(m) = msg {
            match m {
                WorkerMsg::Check {
                    uri,
                    version,
                    text,
                    file_ctx,
                } => {
                    let key = uri.as_str().to_owned();
                    pending.insert(
                        key,
                        PendingJob {
                            uri,
                            version,
                            text,
                            file_ctx: *file_ctx,
                            due_at: Instant::now() + delay,
                        },
                    );
                }
                WorkerMsg::Clear(uri) => {
                    pending.remove(uri.as_str());
                    publish_diagnostics(&lsp_sender, uri, Vec::new(), None);
                }
                WorkerMsg::Shutdown => break,
            }
            // Drain any already-queued messages before running due jobs
            while let Ok(extra) = rx.try_recv() {
                match extra {
                    WorkerMsg::Check {
                        uri,
                        version,
                        text,
                        file_ctx,
                    } => {
                        let key = uri.as_str().to_owned();
                        pending.insert(
                            key,
                            PendingJob {
                                uri,
                                version,
                                text,
                                file_ctx: *file_ctx,
                                due_at: Instant::now() + delay,
                            },
                        );
                    }
                    WorkerMsg::Clear(uri) => {
                        pending.remove(uri.as_str());
                        publish_diagnostics(&lsp_sender, uri, Vec::new(), None);
                    }
                    WorkerMsg::Shutdown => return,
                }
            }
        }

        let now = Instant::now();
        let due_keys: Vec<String> = pending
            .iter()
            .filter(|(_, job)| job.due_at <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in due_keys {
            let Some(job) = pending.remove(&key) else {
                continue;
            };
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                if is_stale(&latest_versions, &key, job.version) {
                    return;
                }
                let Some(diags) = run_clang_diagnostics(&tools, &job.uri, &job.text, &job.file_ctx)
                else {
                    return;
                };
                if is_stale(&latest_versions, &key, job.version) {
                    return;
                }

                let lsp_diags: Vec<Diagnostic> =
                    diags.iter().map(|d| d.diagnostic.clone()).collect();
                if let Ok(mut guard) = cache.lock() {
                    guard.insert(key, diags);
                }
                publish_diagnostics(&lsp_sender, job.uri, lsp_diags, Some(job.version));
            })) && !logged_panic
            {
                logged_panic = true;
                let msg = crate::server::panic_payload_message(&*payload);
                eprintln!("riscv-asm-lsp: panic in diagnostics worker: {msg}");
            }
        }
    }
}

fn is_stale(latest_versions: &Arc<Mutex<HashMap<String, i32>>>, key: &str, version: i32) -> bool {
    latest_versions
        .lock()
        .map(|m| m.get(key).copied() != Some(version))
        .unwrap_or(false)
}

fn publish_diagnostics(
    sender: &MessageSink,
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
) {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version,
    };
    if let Ok(val) = serde_json::to_value(params) {
        sender(Message::Notification(Notification::new(
            "textDocument/publishDiagnostics".to_owned(),
            val,
        )));
    }
}

fn run_clang_diagnostics(
    tools: &ToolRunner,
    uri: &Uri,
    text: &str,
    file_ctx: &FileContext,
) -> Option<Vec<StoredDiagnostic>> {
    let path = uri_to_path(uri);

    let mut args = file_ctx.clang_flags.clone();
    if let Some(dir) = path.as_deref().and_then(|p| p.parent()) {
        args.push("-iquote".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.push("-x".to_owned());
    args.push(if file_ctx.preprocessed {
        "assembler-with-cpp".to_owned()
    } else {
        "assembler".to_owned()
    });
    args.extend([
        "-c".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-".to_owned(),
    ]);

    let output = tools.run_clang(&args, text, file_ctx.working_dir.as_deref())?;
    Some(parse_clang_output_with_ctx(
        text,
        &output.stderr,
        path.as_deref(),
        Some(uri),
        Some(file_ctx),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagKind {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone)]
struct ClangDiagLine<'a> {
    file: &'a str,
    line: u32,
    col: u32,
    kind: DiagKind,
    message: &'a str,
}

pub fn parse_clang_output(text: &str, stderr: &str) -> Vec<StoredDiagnostic> {
    parse_clang_output_with_ctx(text, stderr, None, None, None)
}

pub fn parse_clang_output_with_ctx(
    text: &str,
    stderr: &str,
    doc_path: Option<&Path>,
    doc_uri: Option<&Uri>,
    file_ctx: Option<&FileContext>,
) -> Vec<StoredDiagnostic> {
    let line_index = LineIndex::new(text);
    let lines: Vec<&str> = stderr.lines().collect();
    let preprocessed = file_ctx.is_none_or(|c| c.preprocessed);
    let defined_macros = if preprocessed {
        collect_cpp_define_lines(text)
    } else {
        Vec::new()
    };
    let self_uri = doc_uri.cloned().or_else(|| doc_path.and_then(path_to_uri));
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32, u32, u32, String)> = HashSet::new();
    let mut pending_includes: Vec<(String, u32)> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        if let Some(inc) = parse_include_header(lines[i]) {
            pending_includes.push(inc);
            i += 1;
            continue;
        }

        let Some(diag) = parse_clang_diag_line(lines[i]) else {
            i += 1;
            continue;
        };

        if diag.kind == DiagKind::Note {
            i += 1;
            continue;
        }

        let severity = match diag.kind {
            DiagKind::Error => DiagnosticSeverity::ERROR,
            DiagKind::Warning => DiagnosticSeverity::WARNING,
            DiagKind::Note => unreachable!(),
        };

        // Collect following `note: while in macro instantiation` lines up to the next diagnostic.
        let mut macro_notes = Vec::new();
        let mut j = i + 1;
        while j < lines.len() {
            if parse_include_header(lines[j]).is_some() {
                break;
            }
            if let Some(next_diag) = parse_clang_diag_line(lines[j]) {
                if next_diag.kind == DiagKind::Note {
                    if next_diag.message.contains("while in macro instantiation") {
                        macro_notes.push(next_diag);
                    }
                    j += 1;
                    continue;
                }
                break;
            }
            j += 1;
        }

        let (range, message, suggestions, related, macro_def_hints) = if diag.file == "<stdin>" {
            let range = word_range_for_clang(&line_index, text, diag.line, diag.col);
            let msg = diag.message.to_owned();
            let suggestions = extract_suggestions(&msg);
            let (related, hints) = if preprocessed && !defined_macros.is_empty() {
                build_macro_def_hints(
                    &line_index,
                    text,
                    range.start.line,
                    diag.col,
                    &msg,
                    &defined_macros,
                    self_uri.as_ref(),
                    doc_path,
                )
            } else {
                (None, Vec::new())
            };
            (range, msg, suggestions, related, hints)
        } else if diag.file == "<instantiation>" {
            if let Some(stdin_note) = macro_notes.iter().rfind(|n| n.file == "<stdin>") {
                let range =
                    word_range_for_clang(&line_index, text, stdin_note.line, stdin_note.col);
                let msg = format!("{} (in macro instantiation)", diag.message);
                (range, msg, Vec::new(), None, Vec::new())
            } else if let Some(ext_note) = macro_notes
                .iter()
                .rfind(|n| n.file != "<instantiation>" && !is_clang_preprocessed_temp(n.file))
            {
                let (range, msg, sugg, rel) = build_included_file_diagnostic(
                    &line_index,
                    text,
                    &pending_includes,
                    ext_note.file,
                    ext_note.line,
                    ext_note.col,
                    &format!("{} (in macro instantiation)", diag.message),
                    doc_path,
                    file_ctx,
                    preprocessed,
                );
                (range, msg, sugg, rel, Vec::new())
            } else {
                let msg = format!("{} (in macro instantiation)", diag.message);
                let pos = Position::new(0, 0);
                (Range::new(pos, pos), msg, Vec::new(), None, Vec::new())
            }
        } else {
            let (range, msg, sugg, rel) = build_included_file_diagnostic(
                &line_index,
                text,
                &pending_includes,
                diag.file,
                diag.line,
                diag.col,
                diag.message,
                doc_path,
                file_ctx,
                preprocessed,
            );
            (range, msg, sugg, rel, Vec::new())
        };

        pending_includes.clear();
        i = j;

        let dedup_key = (
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
            message.clone(),
        );
        if !seen.insert(dedup_key) {
            continue;
        }

        out.push(StoredDiagnostic {
            diagnostic: Diagnostic {
                range,
                severity: Some(severity),
                source: Some("clang".to_owned()),
                message,
                related_information: related,
                ..Diagnostic::default()
            },
            suggestions,
            macro_def_hints,
        });
    }

    out
}

#[allow(clippy::too_many_arguments)]
fn build_included_file_diagnostic(
    line_index: &LineIndex,
    text: &str,
    pending_includes: &[(String, u32)],
    err_file: &str,
    err_line: u32,
    err_col: u32,
    err_msg: &str,
    doc_path: Option<&Path>,
    file_ctx: Option<&FileContext>,
    preprocessed: bool,
) -> (
    Range,
    String,
    Vec<String>,
    Option<Vec<DiagnosticRelatedInformation>>,
) {
    let target_line =
        resolve_include_line_for_error(text, pending_includes, err_file, preprocessed);
    let range = trimmed_line_range(line_index, text, target_line);
    let display_name = Path::new(err_file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(err_file);
    let message = format!("in included file `{display_name}:{err_line}:{err_col}`: {err_msg}");
    let related = resolve_related_uri(err_file, doc_path, file_ctx).map(|rel_uri| {
        let pos = Position::new(err_line.saturating_sub(1), err_col.saturating_sub(1));
        vec![DiagnosticRelatedInformation {
            location: Location::new(rel_uri, Range::new(pos, pos)),
            message: err_msg.to_owned(),
        }]
    });
    (range, message, Vec::new(), related)
}

fn word_range_for_clang(
    line_index: &LineIndex,
    text: &str,
    line_1based: u32,
    col_1based: u32,
) -> Range {
    let offset = line_index.clang_pos_to_offset(text, line_1based, col_1based);
    if let Some((ds, de)) = delimited_token_range_at(text, line_index, line_1based, offset) {
        return line_index.byte_span_to_range(text, ds, de);
    }
    let (ws, we) = word_range_at(text, offset);
    if ws < we {
        line_index.byte_span_to_range(text, ws, we)
    } else {
        let pos = line_index.offset_to_position(text, offset);
        Range::new(pos, pos)
    }
}

fn delimited_token_range_at(
    text: &str,
    line_index: &LineIndex,
    line_1based: u32,
    offset: usize,
) -> Option<(usize, usize)> {
    let line_idx = line_1based.saturating_sub(1) as usize;
    let (_, content_end, _) = line_index.line_bounds(text, line_idx);
    if offset >= content_end {
        return None;
    }
    let bytes = text.as_bytes();
    let opener = *bytes.get(offset)?;
    let closer = match opener {
        b'"' => b'"',
        b'<' => b'>',
        _ => return None,
    };

    let mut i = offset + 1;
    let mut end = content_end;
    while i < content_end {
        if opener == b'"' && bytes[i] == b'\\' {
            i = (i + 2).min(content_end);
            continue;
        }
        if bytes[i] == closer {
            end = i + 1;
            break;
        }
        i += 1;
    }
    let end = clamp_to_char_boundary(text, end);
    (offset < end).then_some((offset, end))
}

fn trimmed_line_range(line_index: &LineIndex, text: &str, line_0based: usize) -> Range {
    let (ls, ce, _) = line_index.line_bounds(text, line_0based);
    let line_str = text.get(ls..ce).unwrap_or("");
    let lead_ws = line_str.len() - line_str.trim_start().len();
    let trail_ws = line_str.len() - line_str.trim_end().len();
    let start = ls + lead_ws;
    let end = ce.saturating_sub(trail_ws).max(start);
    line_index.byte_span_to_range(text, start, end)
}

fn parse_include_header(line: &str) -> Option<(String, u32)> {
    let rest = line
        .strip_prefix("In file included from ")
        .or_else(|| line.strip_prefix("Included from "))?;
    let trimmed = rest.trim_end_matches([':', ',', ' ', '\t']);
    let (file, line_str) = trimmed.rsplit_once(':')?;
    let line_1 = line_str.trim().parse::<u32>().ok()?;
    Some((file.to_owned(), line_1))
}

fn parse_clang_diag_line(line: &str) -> Option<ClangDiagLine<'_>> {
    let markers = [
        (": fatal error: ", DiagKind::Error),
        (": error: ", DiagKind::Error),
        (": warning: ", DiagKind::Warning),
        (": note: ", DiagKind::Note),
    ];
    let mut best: Option<(usize, usize, DiagKind)> = None;
    for &(pat, kind) in &markers {
        if let Some(idx) = line.find(pat)
            && best.is_none_or(|(b_idx, _, _)| idx < b_idx)
        {
            best = Some((idx, pat.len(), kind));
        }
    }
    let (marker_idx, marker_len, kind) = best?;
    let loc = &line[..marker_idx];
    let message = line[marker_idx + marker_len..].trim();
    let (file_and_line, col_str) = loc.rsplit_once(':')?;
    let (file, line_str) = file_and_line.rsplit_once(':')?;
    let line_1 = line_str.trim().parse::<u32>().ok()?;
    let col_1 = col_str.trim().parse::<u32>().ok()?;
    Some(ClangDiagLine {
        file,
        line: line_1,
        col: col_1,
        kind,
        message,
    })
}

fn is_clang_preprocessed_temp(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("--") && n.ends_with(".s"))
}

fn resolve_include_line_for_error(
    text: &str,
    pending_includes: &[(String, u32)],
    err_file: &str,
    preprocessed: bool,
) -> usize {
    if let Some((_, line_1)) = pending_includes.iter().rev().find(|(f, _)| f == "<stdin>") {
        return line_1.saturating_sub(1) as usize;
    }

    // With `-x assembler-with-cpp`, `Included from` starts from clang's temp preprocessed file
    // (`/tmp/--XXXXXX.s`), so find the outermost real included file and match it in source.
    let target_file = pending_includes
        .iter()
        .find(|(f, _)| f != "<stdin>" && !is_clang_preprocessed_temp(f))
        .map(|(f, _)| f.as_str())
        .unwrap_or(err_file);

    find_include_line_in_source(text, target_file, preprocessed).unwrap_or(0)
}

fn find_include_line_in_source(text: &str, target_file: &str, preprocessed: bool) -> Option<usize> {
    let target_norm = target_file.replace('\\', "/");
    let target_name = Path::new(&target_norm)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&target_norm);

    let mut fallback = None;
    for (idx, line) in text.lines().enumerate() {
        let Some(inc_target) = extract_include_target(line, preprocessed) else {
            continue;
        };
        let inc_norm = inc_target.replace('\\', "/");
        if target_norm == inc_norm || target_norm.ends_with(&format!("/{inc_norm}")) {
            return Some(idx);
        }
        let inc_name = Path::new(&inc_norm)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&inc_norm);
        if fallback.is_none() && inc_name == target_name {
            fallback = Some(idx);
        }
    }
    fallback
}

fn extract_include_target(line: &str, preprocessed: bool) -> Option<String> {
    let trimmed = line.trim();
    if let Some(after) = trimmed.strip_prefix(".include")
        && after.chars().next().is_some_and(char::is_whitespace)
    {
        let inner = after.trim_start().strip_prefix('"')?;
        let end = inner.find('"')?;
        return Some(inner[..end].to_owned());
    }
    if preprocessed {
        return parse_cpp_include_line(line).map(|(target, _)| target);
    }
    None
}

struct CppDefineLine {
    name: String,
    def_line: u32,
    value: Option<String>,
    is_function_like: bool,
}

fn collect_cpp_define_lines(text: &str) -> Vec<CppDefineLine> {
    let mut defs = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if let Some(parsed) = parse_cpp_define_line(line) {
            defs.push(CppDefineLine {
                name: parsed.name.to_owned(),
                def_line: idx as u32,
                value: parsed.value,
                is_function_like: parsed.is_function_like,
            });
        }
    }
    defs
}

#[allow(clippy::too_many_arguments)]
fn build_macro_def_hints(
    line_index: &LineIndex,
    text: &str,
    diag_line: u32,
    diag_col_1based: u32,
    diag_message: &str,
    defined_macros: &[CppDefineLine],
    doc_uri: Option<&Uri>,
    doc_path: Option<&Path>,
) -> (Option<Vec<DiagnosticRelatedInformation>>, Vec<MacroDefHint>) {
    let (ls, ce, _) = line_index.line_bounds(text, diag_line as usize);
    let raw_line = text.get(ls..ce).unwrap_or("");
    if classify_hash_line(raw_line).is_some() {
        return (None, Vec::new());
    }
    let code = strip_asm_comment_for_ident(raw_line);

    let mut latest_by_name: HashMap<&str, (&CppDefineLine, usize)> = HashMap::new();
    for def in defined_macros {
        if def.def_line < diag_line
            && let Some(ref_col) = find_ident_word_col(code, &def.name)
        {
            latest_by_name.insert(def.name.as_str(), (def, ref_col));
        }
    }
    if latest_by_name.is_empty() {
        return (None, Vec::new());
    }

    let mut matched: Vec<(&CppDefineLine, usize)> = latest_by_name.into_values().collect();
    matched.sort_by(|a, b| {
        a.0.def_line
            .cmp(&b.0.def_line)
            .then_with(|| a.0.name.cmp(&b.0.name))
    });

    let file_kind = if doc_path
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        == Some("S")
    {
        "`.S` files"
    } else {
        "preprocessed files"
    };
    let error_matches_expansion = is_macro_expansion_error_message(diag_message);

    let mut hints = Vec::new();
    let mut related = Vec::with_capacity(matched.len());
    for (def, ref_col) in matched {
        let col_in_expansion = diag_col_1based >= (ref_col as u32).saturating_add(1);
        let suggest_comment_fix = error_matches_expansion
            && col_in_expansion
            && looks_like_prose_define(def.value.as_deref(), def.is_function_like);

        if let Some(uri) = doc_uri {
            let def_range = trimmed_line_range(line_index, text, def.def_line as usize);
            let line_num = def.def_line + 1;
            let message = if suggest_comment_fix {
                match &def.value {
                    Some(val) => format!(
                        "`{}` expands to `{val}` (defined by the preprocessor directive on line {line_num}); in {file_kind} a `#` line starting with a directive name is not a comment",
                        def.name
                    ),
                    None => format!(
                        "`{}` is defined by the preprocessor directive on line {line_num}; in {file_kind} a `#` line starting with a directive name is not a comment",
                        def.name
                    ),
                }
            } else {
                match &def.value {
                    Some(val) => format!(
                        "`{}` expands to `{val}` (defined on line {line_num})",
                        def.name
                    ),
                    None => format!("`{}` is defined on line {line_num}", def.name),
                }
            };
            related.push(DiagnosticRelatedInformation {
                location: Location::new(uri.clone(), def_range),
                message,
            });
        }

        if suggest_comment_fix {
            hints.push(MacroDefHint {
                macro_name: def.name.clone(),
                def_line: def.def_line,
            });
        }
    }

    let rel_opt = (!related.is_empty()).then_some(related);
    (rel_opt, hints)
}

fn is_macro_expansion_error_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("unexpected token")
        || lower.contains("unknown token in expression")
        || lower.contains("invalid operand")
        || lower.contains("unrecognized instruction mnemonic")
}

fn looks_like_prose_define(value: Option<&str>, is_function_like: bool) -> bool {
    if is_function_like {
        return false;
    }
    let Some(val) = value else {
        return false;
    };
    if val.chars().any(|c| {
        matches!(
            c,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '+'
                | '-'
                | '*'
                | '/'
                | '%'
                | '&'
                | '|'
                | '^'
                | '~'
                | '<'
                | '>'
                | '='
                | '!'
                | '?'
                | '\\'
        )
    }) {
        return false;
    }
    let mut cur_run = 0usize;
    let mut max_run = 0usize;
    for raw_tok in val.split_whitespace() {
        let tok = raw_tok.trim_end_matches([',', '.']);
        let is_plain_word = !tok.is_empty()
            && tok.bytes().all(|b| b.is_ascii_alphabetic())
            && tok.bytes().any(|b| b.is_ascii_lowercase());
        if is_plain_word {
            cur_run += 1;
            max_run = max_run.max(cur_run);
        } else {
            cur_run = 0;
        }
    }
    max_run >= 2
}

fn strip_asm_comment_for_ident(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if b == b'\'' {
            in_single = true;
        } else if b == b'"' {
            in_double = true;
        } else if b == b'#' || (b == b'/' && bytes.get(i + 1) == Some(&b'/')) {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn find_ident_word_col(haystack: &str, ident: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut i = 0;
    while let Some(pos) = haystack[i..].find(ident) {
        let start = i + pos;
        let end = start + ident.len();
        let before_ok =
            start == 0 || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_');
        let after_ok =
            end >= bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
        if before_ok && after_ok {
            return Some(start);
        }
        i = start + 1;
    }
    None
}

fn resolve_related_uri(
    err_file: &str,
    doc_path: Option<&Path>,
    file_ctx: Option<&FileContext>,
) -> Option<Uri> {
    let raw = Path::new(err_file);
    let resolved: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else if let Some(wd) = file_ctx.and_then(|c| c.working_dir.as_deref()) {
        wd.join(raw)
    } else {
        let dir = doc_path.and_then(|p| p.parent())?;
        dir.join(raw)
    };
    let display = if let Some(ctx) = file_ctx {
        display_path_for_client(
            &resolved,
            ctx.client_root.as_deref(),
            ctx.canon_root.as_deref(),
        )
    } else {
        resolved
    };
    path_to_uri(&display)
}

fn extract_suggestions(message: &str) -> Vec<String> {
    let Some(idx) = message.find("did you mean:") else {
        return Vec::new();
    };
    let rest = &message[idx + "did you mean:".len()..];
    let body = rest.split('?').next().unwrap_or(rest);
    body.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    #[test]
    fn parses_clang_errors_with_multibyte_prefix_and_suggestions() {
        let src = "/* 日本語 */ mvi a0, 1\n    adid a0, a1, 1\n";
        let byte_col = "/* 日本語 */ ".len() + 1;
        let stderr = format!(
            "<stdin>:1:{byte_col}: error: unrecognized instruction mnemonic, did you mean: mv?\n\
             <stdin>:2:5: error: unrecognized instruction mnemonic, did you mean: add, addi?\n"
        );
        let diags = parse_clang_output(src, &stderr);
        assert_eq!(diags.len(), 2);

        // Line 0: "/* 日本語 */ " is 10 UTF-16 code units!
        assert_eq!(diags[0].diagnostic.range.start, Position::new(0, 10));
        assert_eq!(diags[0].diagnostic.range.end, Position::new(0, 13));
        assert_eq!(diags[0].suggestions, vec!["mv"]);

        assert_eq!(diags[1].diagnostic.range.start, Position::new(1, 4));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(1, 8));
        assert_eq!(diags[1].suggestions, vec!["add", "addi"]);
    }

    #[test]
    fn parses_macro_instantiation_and_included_file_errors() {
        let src = "\
    .include \"bad.inc\"
#include \"bad.h\"
.macro TWICE
    nop
    mvx a0, a1
.endm
    TWICE
";
        let stderr = "\
Included from /tmp/--0bca6a.s:8:
/tmp/proj/bad.inc:1:5: error: unrecognized instruction mnemonic, did you mean: mv?
    mov a0, a1
    ^
In file included from <stdin>:2:
/tmp/proj/bad.h:1:2: error: bad header
    1 | #error bad header
      |  ^
<instantiation>:2:5: error: unrecognized instruction mnemonic, did you mean: mv?
    mvx a0, a1
    ^
<stdin>:7:5: note: while in macro instantiation
    TWICE
    ^
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 3, "diags: {diags:?}");

        // 0: `.include "bad.inc"` on line 0
        assert_eq!(diags[0].diagnostic.range.start.line, 0);
        assert!(diags[0].diagnostic.message.contains("bad.inc:1:5"));
        assert!(diags[0].diagnostic.related_information.is_some());

        // 1: `#include "bad.h"` on line 1
        assert_eq!(diags[1].diagnostic.range.start.line, 1);
        assert!(diags[1].diagnostic.message.contains("bad.h:1:2"));
        assert!(diags[1].diagnostic.message.contains("bad header"));

        // 2: `TWICE` on line 6
        assert_eq!(diags[2].diagnostic.range.start, Position::new(6, 4));
        assert_eq!(diags[2].diagnostic.range.end, Position::new(6, 9));
        assert!(
            diags[2]
                .diagnostic
                .message
                .contains("in macro instantiation")
        );
    }

    #[test]
    fn parses_nested_macros_and_nested_asm_includes() {
        let src = "\
    .include \"mid.inc\"
.macro INNER
    mvx a0, a1
.endm
.macro OUTER
    INNER
.endm
    OUTER
";
        let stderr = "\
Included from /tmp/--490bbc.s:8:
Included from /tmp/proj/mid.inc:1:
/tmp/proj/bad.inc:1:5: warning: some warning in included file
    nop
    ^
<instantiation>:1:1: error: unrecognized instruction mnemonic
mvx a0, a1
^
<instantiation>:1:1: note: while in macro instantiation
INNER
^
<stdin>:8:5: note: while in macro instantiation
    OUTER
    ^
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 2, "diags: {diags:?}");
        assert_eq!(diags[0].diagnostic.range.start.line, 0);
        assert_eq!(
            diags[0].diagnostic.severity,
            Some(DiagnosticSeverity::WARNING)
        );
        assert!(diags[0].diagnostic.message.contains("bad.inc:1:5"));

        assert_eq!(diags[1].diagnostic.range.start, Position::new(7, 4));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(7, 9));
    }

    #[test]
    fn missing_include_file_covers_delimited_filename() {
        let src =
            "    .include \"not_there.inc\"\n#include \"missing.h\"\n#include <sys_missing.h>\n";
        let stderr = "\
<stdin>:1:14: error: Could not find include file 'not_there.inc'
    .include \"not_there.inc\"
             ^
<stdin>:2:10: fatal error: 'missing.h' file not found
    2 | #include \"missing.h\"
      |          ^~~~~~~~~~~
<stdin>:3:10: fatal error: 'sys_missing.h' file not found
    3 | #include <sys_missing.h>
      |          ^~~~~~~~~~~~~~~
";
        let diags = parse_clang_output(src, stderr);
        assert_eq!(diags.len(), 3, "diags: {diags:?}");
        assert_eq!(diags[0].diagnostic.range.start, Position::new(0, 13));
        assert_eq!(diags[0].diagnostic.range.end, Position::new(0, 28));
        assert_eq!(diags[1].diagnostic.range.start, Position::new(1, 9));
        assert_eq!(diags[1].diagnostic.range.end, Position::new(1, 20));
        assert_eq!(diags[2].diagnostic.range.start, Position::new(2, 9));
        assert_eq!(diags[2].diagnostic.range.end, Position::new(2, 24));
    }

    #[test]
    fn attaches_related_info_and_macro_hint_for_spaced_define_error() {
        let src = "# define X 5 is the size\n_start:\n    li a0, X\n";
        let stderr =
            "<stdin>:3:14: error: unexpected token\n    li a0, 5 is the size\n             ^\n";
        let uri: Uri = "file:///tmp/spaced.S".parse().expect("uri");
        let diags = parse_clang_output_with_ctx(
            src,
            stderr,
            Some(Path::new("/tmp/spaced.S")),
            Some(&uri),
            None,
        );
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].diagnostic.range.start.line, 2);
        assert_eq!(diags[0].macro_def_hints.len(), 1);
        assert_eq!(diags[0].macro_def_hints[0].macro_name, "X");
        assert_eq!(diags[0].macro_def_hints[0].def_line, 0);
        let rel = diags[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("related_information");
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].location.uri, uri);
        assert_eq!(rel[0].location.range.start.line, 0);
        assert!(rel[0].message.contains("`X` expands to `5 is the size`"));
        assert!(rel[0].message.contains("line 1"));
        assert!(rel[0].message.contains("not a comment"));
    }

    #[test]
    fn linux_style_macro_error_gets_neutral_related_info_without_quickfix() {
        let src = "\
#ifdef BIG
# define CZ clz
#else
# define CZ ctz
#endif
    CZ a0, a1
";
        let stderr = "<stdin>:6:5: error: instruction requires the following: 'Zbb' (Basic Bit-Manipulation)\n    ctz a0, a1\n    ^\n";
        let uri: Uri = "file:///tmp/cz.S".parse().expect("uri");
        let diags = parse_clang_output_with_ctx(
            src,
            stderr,
            Some(Path::new("/tmp/cz.S")),
            Some(&uri),
            None,
        );
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].macro_def_hints.is_empty(),
            "unexpected quickfix hint for real macro: {:?}",
            diags[0].macro_def_hints
        );
        let rel = diags[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("neutral related_information");
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].location.range.start.line, 3);
        assert_eq!(rel[0].message, "`CZ` expands to `ctz` (defined on line 4)");

        // Even with a prose define, an error whose column is before the macro usage stays neutral
        let src2 = "# define X 5 is the size\n_start:\n    mvi a0, X\n";
        let stderr2 = "<stdin>:3:5: error: unrecognized instruction mnemonic, did you mean: mv?\n    mvi a0, 5 is the size\n    ^\n";
        let uri2: Uri = "file:///tmp/col_before.S".parse().expect("uri");
        let diags2 = parse_clang_output_with_ctx(
            src2,
            stderr2,
            Some(Path::new("/tmp/col_before.S")),
            Some(&uri2),
            None,
        );
        assert_eq!(diags2.len(), 1);
        assert!(diags2[0].macro_def_hints.is_empty());
        let rel2 = diags2[0]
            .diagnostic
            .related_information
            .as_ref()
            .expect("neutral related_information");
        assert_eq!(
            rel2[0].message,
            "`X` expands to `5 is the size` (defined on line 1)"
        );
    }
}
