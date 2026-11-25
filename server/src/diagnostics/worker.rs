use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification};
use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};

use crate::config::{DiagnosticsConfig, ExternalDiagnosticsMode};
use crate::context::{FileContext, uri_key};
use crate::data::Database;
use crate::tools::ToolRunner;

use super::builtin::{filter_suppressed, run_builtin_diagnostics};
use super::clang::run_clang_diagnostics;
use super::{DiagnosticsCache, MessageSink};

enum WorkerMsg {
    Check {
        uri: Uri,
        version: i32,
        text: String,
        file_ctx: Box<FileContext>,
        immediate: bool,
    },
    Clear(Uri),
    InvalidateAll,
    UpdateConfig(DiagnosticsConfig),
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
        config: DiagnosticsConfig,
        db: Arc<Database>,
        tools: Arc<ToolRunner>,
        lsp_sender: MessageSink,
    ) -> Self {
        let latest_versions: Arc<Mutex<HashMap<String, i32>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let cache: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

        let (tx, rx) = mpsc::channel();
        let versions_bg = Arc::clone(&latest_versions);
        let cache_bg = Arc::clone(&cache);
        let delay = Duration::from_millis(config.delay_ms);

        let handle = thread::spawn(move || {
            let mut runner = DiagnosticsRunner {
                rx,
                enable: config.enable,
                delay,
                external: config.external,
                suppress: config.suppress,
                db,
                tools,
                lsp_sender,
                latest_versions: versions_bg,
                cache: cache_bg,
                pending: HashMap::new(),
                last_diagnosed: HashMap::new(),
                cache_uris: HashMap::new(),
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
        self.schedule_with_mode(uri, version, text, file_ctx, false);
    }

    pub fn schedule_immediate(&self, uri: Uri, version: i32, text: String, file_ctx: FileContext) {
        self.schedule_with_mode(uri, version, text, file_ctx, true);
    }

    fn schedule_with_mode(
        &self,
        uri: Uri,
        version: i32,
        text: String,
        file_ctx: FileContext,
        immediate: bool,
    ) {
        if let Ok(mut map) = self.latest_versions.lock() {
            map.insert(uri_key(&uri), version);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Check {
                uri,
                version,
                text,
                file_ctx: Box::new(file_ctx),
                immediate,
            });
        }
    }

    pub fn invalidate_all(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::InvalidateAll);
        }
    }

    pub fn update_config(&self, config: DiagnosticsConfig) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::UpdateConfig(config));
        }
    }

    pub fn clear(&self, uri: &Uri) {
        let key = uri_key(uri);
        let had_version = self
            .latest_versions
            .lock()
            .is_ok_and(|mut map| map.remove(&key).is_some());
        let had_cache = self
            .cache
            .lock()
            .is_ok_and(|mut cache| cache.remove(&key).is_some());
        if (had_version || had_cache)
            && let Some(tx) = &self.tx
        {
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
    force: bool,
}

struct DiagnosticsRunner {
    rx: mpsc::Receiver<WorkerMsg>,
    enable: bool,
    delay: Duration,
    external: ExternalDiagnosticsMode,
    suppress: Vec<String>,
    db: Arc<Database>,
    tools: Arc<ToolRunner>,
    lsp_sender: MessageSink,
    latest_versions: Arc<Mutex<HashMap<String, i32>>>,
    cache: DiagnosticsCache,
    pending: HashMap<String, PendingJob>,
    last_diagnosed: HashMap<String, i32>,
    cache_uris: HashMap<String, Uri>,
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

            if !self.run_due_jobs() {
                return;
            }
        }
    }

    fn wait_next_message(&self) -> WaitResult {
        if self.enable
            && let Some(next_due) = self.pending.values().map(|j| j.due_at).min()
        {
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
                immediate,
            } => {
                if !self.enable {
                    return true;
                }
                let key = uri_key(&uri);
                if immediate && self.last_diagnosed.get(&key).copied() == Some(version) {
                    return true;
                }
                let due_at = if immediate {
                    Instant::now()
                } else {
                    Instant::now() + self.delay
                };
                self.pending.insert(
                    key,
                    PendingJob {
                        uri,
                        version,
                        text,
                        file_ctx: *file_ctx,
                        due_at,
                        force: false,
                    },
                );
                true
            }
            WorkerMsg::Clear(uri) => {
                let key = uri_key(&uri);
                self.pending.remove(&key);
                self.last_diagnosed.remove(&key);
                self.cache_uris.remove(&key);
                publish_diagnostics(&self.lsp_sender, uri, Vec::new(), None);
                true
            }
            WorkerMsg::InvalidateAll => {
                self.last_diagnosed.clear();
                for job in self.pending.values_mut() {
                    job.force = true;
                }
                true
            }
            WorkerMsg::UpdateConfig(cfg) => {
                self.enable = cfg.enable;
                self.delay = Duration::from_millis(cfg.delay_ms);
                self.external = cfg.external;
                self.suppress = cfg.suppress;
                self.last_diagnosed.clear();
                if !cfg.enable {
                    let keys: Vec<String> = if let Ok(mut guard) = self.cache.lock() {
                        guard.drain().map(|(k, _)| k).collect()
                    } else {
                        Vec::new()
                    };
                    self.pending.clear();
                    for k in keys {
                        if let Some(uri) =
                            self.cache_uris.remove(&k).or_else(|| k.parse::<Uri>().ok())
                        {
                            publish_diagnostics(&self.lsp_sender, uri, Vec::new(), None);
                        }
                    }
                }
                true
            }
            WorkerMsg::Shutdown => false,
        }
    }

    fn run_due_jobs(&mut self) -> bool {
        if !self.enable {
            return true;
        }
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                if !self.apply_message(msg) {
                    return false;
                }
            }
            if !self.enable {
                return true;
            }
            let now = Instant::now();
            let Some(key) = self
                .pending
                .iter()
                .filter(|(_, job)| job.due_at <= now)
                .min_by_key(|(_, job)| job.due_at)
                .map(|(k, _)| k.clone())
            else {
                return true;
            };
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

    fn execute_job(&mut self, key: String, job: PendingJob) {
        if is_stale(&self.latest_versions, &key, job.version) {
            return;
        }
        if !job.force && self.last_diagnosed.get(&key).copied() == Some(job.version) {
            return;
        }
        let diags = match self.external {
            ExternalDiagnosticsMode::Off => run_builtin_diagnostics(
                &job.uri,
                &job.text,
                &job.file_ctx,
                &self.db,
                &self.suppress,
            ),
            ExternalDiagnosticsMode::Auto | ExternalDiagnosticsMode::On => {
                if let Some(ext) =
                    run_clang_diagnostics(&self.tools, &job.uri, &job.text, &job.file_ctx)
                {
                    filter_suppressed(ext, &self.suppress)
                } else {
                    run_builtin_diagnostics(
                        &job.uri,
                        &job.text,
                        &job.file_ctx,
                        &self.db,
                        &self.suppress,
                    )
                }
            }
        };
        if is_stale(&self.latest_versions, &key, job.version) {
            return;
        }

        let lsp_diags: Vec<Diagnostic> = diags.iter().map(|d| d.diagnostic.clone()).collect();
        self.last_diagnosed.insert(key.clone(), job.version);
        self.cache_uris.insert(key.clone(), job.uri.clone());
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
