use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};

use crate::context::FileContext;
use crate::tools::ToolRunner;

use super::clang::run_clang_diagnostics;
use super::{DiagnosticsCache, MessageSink};

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
            let mut runner = DiagnosticsRunner {
                rx,
                delay,
                tools,
                lsp_sender,
                latest_versions: versions_bg,
                cache: cache_bg,
                pending: HashMap::new(),
                logged_panic: false,
            };
            runner.run();
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

struct DiagnosticsRunner {
    rx: mpsc::Receiver<WorkerMsg>,
    delay: Duration,
    tools: Arc<ToolRunner>,
    lsp_sender: MessageSink,
    latest_versions: Arc<Mutex<HashMap<String, i32>>>,
    cache: DiagnosticsCache,
    pending: HashMap<String, PendingJob>,
    logged_panic: bool,
}

impl DiagnosticsRunner {
    fn run(&mut self) {
        loop {
            match self.wait_next_message() {
                WaitResult::Message(msg) => {
                    if !self.apply_message(msg) {
                        return;
                    }
                    // Drain any already-queued messages before running due jobs
                    while let Ok(extra) = self.rx.try_recv() {
                        if !self.apply_message(extra) {
                            return;
                        }
                    }
                }
                WaitResult::DueReady => {}
                WaitResult::Disconnected => return,
            }

            self.run_due_jobs();
        }
    }

    fn wait_next_message(&self) -> WaitResult {
        if let Some(next_due) = self.pending.values().map(|j| j.due_at).min() {
            let now = Instant::now();
            if next_due <= now {
                return WaitResult::DueReady;
            }
            match self
                .rx
                .recv_timeout(next_due.saturating_duration_since(now))
            {
                Ok(m) => WaitResult::Message(m),
                Err(mpsc::RecvTimeoutError::Timeout) => WaitResult::DueReady,
                Err(mpsc::RecvTimeoutError::Disconnected) => WaitResult::Disconnected,
            }
        } else {
            match self.rx.recv() {
                Ok(m) => WaitResult::Message(m),
                Err(_) => WaitResult::Disconnected,
            }
        }
    }

    fn apply_message(&mut self, msg: WorkerMsg) -> bool {
        match msg {
            WorkerMsg::Check {
                uri,
                version,
                text,
                file_ctx,
            } => {
                let key = uri.as_str().to_owned();
                self.pending.insert(
                    key,
                    PendingJob {
                        uri,
                        version,
                        text,
                        file_ctx: *file_ctx,
                        due_at: Instant::now() + self.delay,
                    },
                );
                true
            }
            WorkerMsg::Clear(uri) => {
                self.pending.remove(uri.as_str());
                publish_diagnostics(&self.lsp_sender, uri, Vec::new(), None);
                true
            }
            WorkerMsg::Shutdown => false,
        }
    }

    fn run_due_jobs(&mut self) {
        let now = Instant::now();
        let due_keys: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, job)| job.due_at <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in due_keys {
            let Some(job) = self.pending.remove(&key) else {
                continue;
            };
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.execute_job(key, job)))
                && !self.logged_panic
            {
                self.logged_panic = true;
                let msg = crate::server::panic_payload_message(&*payload);
                eprintln!("riscv-asm-lsp: panic in diagnostics worker: {msg}");
            }
        }
    }

    fn execute_job(&self, key: String, job: PendingJob) {
        if is_stale(&self.latest_versions, &key, job.version) {
            return;
        }
        let Some(diags) = run_clang_diagnostics(&self.tools, &job.uri, &job.text, &job.file_ctx)
        else {
            return;
        };
        if is_stale(&self.latest_versions, &key, job.version) {
            return;
        }

        let lsp_diags: Vec<Diagnostic> = diags.iter().map(|d| d.diagnostic.clone()).collect();
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(key, diags);
        }
        publish_diagnostics(&self.lsp_sender, job.uri, lsp_diags, Some(job.version));
    }
}

enum WaitResult {
    Message(WorkerMsg),
    DueReady,
    Disconnected,
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
