use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{Diagnostic, DiagnosticSeverity, PublishDiagnosticsParams, Range, Uri};

use crate::context::{FileContext, uri_to_path};
use crate::position::{LineIndex, word_range_at};
use crate::tools::ToolRunner;

pub type MessageSink = Arc<dyn Fn(Message) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct StoredDiagnostic {
    pub diagnostic: Diagnostic,
    pub suggestions: Vec<String>,
}

pub type DiagnosticsCache = Arc<Mutex<HashMap<String, Vec<StoredDiagnostic>>>>;

enum WorkerMsg {
    Check {
        uri: Uri,
        version: i32,
        text: String,
        file_ctx: FileContext,
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
                file_ctx,
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
                            file_ctx,
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
                                file_ctx,
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
            if is_stale(&latest_versions, &key, job.version) {
                continue;
            }
            let Some(diags) = run_clang_diagnostics(&tools, &job.uri, &job.text, &job.file_ctx)
            else {
                continue;
            };
            if is_stale(&latest_versions, &key, job.version) {
                continue;
            }

            let lsp_diags: Vec<Diagnostic> = diags.iter().map(|d| d.diagnostic.clone()).collect();
            if let Ok(mut guard) = cache.lock() {
                guard.insert(key, diags);
            }
            publish_diagnostics(&lsp_sender, job.uri, lsp_diags, Some(job.version));
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
    let is_plain_s = path
        .as_deref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        == Some("s");

    let mut args = file_ctx.clang_flags.clone();
    if let Some(dir) = path.as_deref().and_then(|p| p.parent()) {
        args.push("-iquote".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.push("-x".to_owned());
    args.push(if is_plain_s {
        "assembler".to_owned()
    } else {
        "assembler-with-cpp".to_owned()
    });
    args.extend([
        "-c".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-".to_owned(),
    ]);

    let output = tools.run_clang(&args, text)?;
    Some(parse_clang_output(text, &output.stderr))
}

pub fn parse_clang_output(text: &str, stderr: &str) -> Vec<StoredDiagnostic> {
    let line_index = LineIndex::new(text);
    let mut out = Vec::new();

    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("<stdin>:") else {
            continue;
        };
        let mut parts = rest.splitn(4, ':');
        let Some(line_str) = parts.next() else {
            continue;
        };
        let Some(col_str) = parts.next() else {
            continue;
        };
        let Some(sev_str) = parts.next() else {
            continue;
        };
        let Some(msg_raw) = parts.next() else {
            continue;
        };

        let Ok(line_1based) = line_str.trim().parse::<u32>() else {
            continue;
        };
        let Ok(col_1based) = col_str.trim().parse::<u32>() else {
            continue;
        };

        let severity = match sev_str.trim() {
            "error" | "fatal error" => DiagnosticSeverity::ERROR,
            "warning" => DiagnosticSeverity::WARNING,
            _ => continue,
        };

        let message = msg_raw.trim().to_owned();
        let offset = line_index.clang_pos_to_offset(text, line_1based, col_1based);
        let (ws, we) = word_range_at(text, offset);
        let range = if ws < we {
            line_index.byte_span_to_range(text, ws, we)
        } else {
            let pos = line_index.offset_to_position(text, offset);
            Range::new(pos, pos)
        };

        let suggestions = extract_suggestions(&message);
        out.push(StoredDiagnostic {
            diagnostic: Diagnostic {
                range,
                severity: Some(severity),
                source: Some("clang".to_owned()),
                message,
                ..Diagnostic::default()
            },
            suggestions,
        });
    }

    out
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
}
