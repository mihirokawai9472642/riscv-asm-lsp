use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use lsp_server::{Connection, ErrorCode, Message, Request, RequestId, Response};
use lsp_types::{
    CodeActionParams, CodeActionProviderCapability, CompletionItem, CompletionOptions,
    CompletionParams, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentHighlightParams, DocumentSymbolParams, FileChangeType,
    FileSystemWatcher, FoldingRangeParams, FoldingRangeProviderCapability, GlobPattern,
    GotoDefinitionParams, HoverParams, HoverProviderCapability, InitializeParams, InlayHintParams,
    OneOf, ReferenceParams, Registration, RegistrationParams, RenameOptions, RenameParams,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, SignatureHelpOptions, SignatureHelpParams, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri, WatchKind, WorkspaceSymbolParams,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::code_action::provide_code_actions;
use crate::completion::{CompletionClientCaps, provide_completion, resolve_completion_item};
use crate::config::{Config, merge_json_value};
use crate::context::{CompileContext, is_linker_script_uri, uri_to_path};
use crate::data::Database;
use crate::diagnostics::{DiagnosticsWorker, MessageSink};
use crate::document::DocumentStore;
use crate::folding::provide_folding_ranges;
use crate::hover::{HoverResolution, HoverWorker, prepare_hover};
use crate::index::WorkspaceIndex;
use crate::inlay_hint::provide_inlay_hints;
use crate::navigation::{
    document_highlight, find_references, goto_definition, prepare_rename, rename_symbol,
};
use crate::semantic_tokens::{legend, provide_semantic_tokens_full, provide_semantic_tokens_range};
use crate::signature_help::provide_signature_help;
use crate::symbols::{document_symbols, workspace_symbols};
use crate::tools::ToolRunner;

const INDEX_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![
                ".".to_owned(),
                "%".to_owned(),
                "\\".to_owned(),
                ",".to_owned(),
                " ".to_owned(),
            ]),
            ..CompletionOptions::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: legend(),
                range: Some(true),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..SemanticTokensOptions::default()
            },
        )),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec![" ".to_owned(), ",".to_owned()]),
            retrigger_characters: Some(vec![",".to_owned()]),
            work_done_progress_options: Default::default(),
        }),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}

pub fn run_stdio() -> Result<i32, Box<dyn std::error::Error>> {
    let db = Arc::new(Database::load()?);
    let (connection, io_threads) = Connection::stdio();

    let caps_json = serde_json::to_value(server_capabilities())?;
    let init_params_json = connection.initialize(caps_json)?;
    let init_params: InitializeParams = serde_json::from_value(init_params_json)?;

    let next_req_id = AtomicI32::new(1);
    let alloc_req_id = || RequestId::from(next_req_id.fetch_add(1, Ordering::Relaxed));

    register_watched_files_if_supported(&connection, &init_params, &alloc_req_id);

    let completion_caps = extract_completion_caps(&init_params);
    let progress_ok = init_params
        .capabilities
        .window
        .as_ref()
        .and_then(|w| w.work_done_progress)
        == Some(true);
    let workspace_roots = extract_workspace_roots(&init_params);
    let init_options_raw = init_params
        .initialization_options
        .unwrap_or(serde_json::Value::Null);
    let config = Config::from_init_options(Some(init_options_raw.clone()));
    let ctx = CompileContext::new_multi(workspace_roots.clone(), config.clone());
    let store = DocumentStore::new().map_err(std::io::Error::other)?;
    let mut index = WorkspaceIndex::new_multi(workspace_roots).map_err(std::io::Error::other)?;
    index.set_excluded_paths(ctx.non_riscv_paths());
    index.start_background_scan_with_id(
        progress_ok.then(|| make_message_sink(&connection)),
        alloc_req_id(),
    );
    let tools = Arc::new(ToolRunner::new(
        config.clang.clone(),
        config.llvm_mc.clone(),
        Some(make_message_sink(&connection)),
    ));
    let diag_worker = DiagnosticsWorker::new(
        config.diagnostics.clone(),
        Arc::clone(&db),
        Arc::clone(&tools),
        make_message_sink(&connection),
    );
    let hover_worker = HoverWorker::new(Arc::clone(&tools), make_message_sink(&connection));

    eprintln!(
        "riscv-asm-lsp started (default_target={}, default_march={}, instructions={}, csrs={})",
        config.default_target,
        config.default_march,
        db.raw.instructions.len(),
        db.raw.csrs.len()
    );

    let mut state = ServerState {
        connection: &connection,
        db: &db,
        ctx,
        init_options_raw,
        store,
        index,
        tools,
        diag_worker,
        hover_worker,
        completion_caps,
        progress_ok,
        next_req_id,
        dirty_docs: HashMap::new(),
        logged_panics: HashSet::new(),
        logged_non_riscv: HashSet::new(),
    };

    let loop_res = state.main_loop();
    // Shut down background workers and drop `state` before `connection` so `LspServerWriter` drains cleanly.
    let ServerState {
        mut index,
        diag_worker,
        hover_worker,
        tools,
        ..
    } = state;
    index.shutdown();
    diag_worker.shutdown();
    hover_worker.shutdown();
    drop(tools);
    drop(connection);

    let (shutdown_received, exit_code) = match loop_res {
        Ok(res) => res,
        Err(err) => {
            let _ = io_threads.join();
            return Err(err);
        }
    };
    if let Err(err) = io_threads.join() {
        if shutdown_received && exit_code == 0 {
            eprintln!("riscv-asm-lsp: non-fatal teardown error after shutdown: {err}");
            return Ok(0);
        }
        if exit_code == 0 {
            return Err(err.into());
        }
    }
    Ok(exit_code)
}

struct ServerState<'a> {
    connection: &'a Connection,
    db: &'a Database,
    ctx: CompileContext,
    init_options_raw: serde_json::Value,
    store: DocumentStore,
    index: WorkspaceIndex,
    tools: Arc<ToolRunner>,
    diag_worker: DiagnosticsWorker,
    hover_worker: HoverWorker,
    completion_caps: CompletionClientCaps,
    progress_ok: bool,
    next_req_id: AtomicI32,
    dirty_docs: HashMap<String, (Uri, Instant)>,
    logged_panics: HashSet<String>,
    logged_non_riscv: HashSet<String>,
}

enum NextMsg {
    Message(Message),
    Timeout,
    Closed,
}

impl ServerState<'_> {
    fn main_loop(&mut self) -> Result<(bool, i32), Box<dyn std::error::Error>> {
        let mut shutdown_received = false;
        let mut exit_code = 0i32;

        loop {
            let msg = match self.next_message() {
                NextMsg::Message(m) => m,
                NextMsg::Timeout => continue,
                NextMsg::Closed => break,
            };
            match msg {
                Message::Request(req) => {
                    if req.method == "shutdown" {
                        if shutdown_received {
                            reply_err(
                                self.connection,
                                req.id,
                                ErrorCode::InvalidRequest,
                                "server is already shutting down".to_owned(),
                            )?;
                        } else {
                            reply_ok(self.connection, req.id, serde_json::Value::Null)?;
                            shutdown_received = true;
                        }
                        continue;
                    }
                    if shutdown_received {
                        reply_err(
                            self.connection,
                            req.id,
                            ErrorCode::InvalidRequest,
                            "server is shutting down".to_owned(),
                        )?;
                        continue;
                    }
                    self.dispatch_request_guarded(req)?;
                }
                Message::Notification(not) => {
                    if not.method == "exit" {
                        exit_code = if shutdown_received { 0 } else { 1 };
                        break;
                    }
                    if !shutdown_received {
                        self.dispatch_notification_guarded(not.method, not.params);
                    }
                }
                Message::Response(_) => {}
            }
        }
        Ok((shutdown_received, exit_code))
    }

    fn next_message(&mut self) -> NextMsg {
        if let Some(next_due) = self.dirty_docs.values().map(|(_, due)| *due).min() {
            let remaining = next_due.saturating_duration_since(Instant::now());
            match self.connection.receiver.recv_timeout(remaining) {
                Ok(m) => NextMsg::Message(m),
                Err(err) if err.is_timeout() => {
                    self.flush_due_dirty_docs();
                    NextMsg::Timeout
                }
                Err(_) => NextMsg::Closed,
            }
        } else {
            match self.connection.receiver.recv() {
                Ok(m) => NextMsg::Message(m),
                Err(_) => NextMsg::Closed,
            }
        }
    }

    fn dispatch_request_guarded(&mut self, req: Request) -> Result<(), Box<dyn std::error::Error>> {
        let id = req.id.clone();
        let method = req.method.clone();
        let connection = self.connection;
        match catch_unwind(AssertUnwindSafe(|| {
            if !matches!(
                method.as_str(),
                "completionItem/resolve" | "textDocument/codeAction" | "textDocument/signatureHelp"
            ) {
                self.flush_all_dirty_docs();
            }
            let store = &self.store;
            if matches!(
                method.as_str(),
                "textDocument/definition"
                    | "textDocument/references"
                    | "textDocument/rename"
                    | "workspace/symbol"
            ) {
                self.index
                    .drain_background_wait(&|p| store.is_open_path(p), Duration::from_millis(200));
            } else {
                self.index.drain_background(&|p| store.is_open_path(p));
            }
            self.handle_request(req)
        })) {
            Ok(res) => res,
            Err(payload) => {
                if self.logged_panics.insert(method.clone()) {
                    let msg = panic_payload_message(&*payload);
                    eprintln!("riscv-asm-lsp: panic while handling request `{method}`: {msg}");
                }
                reply_err(
                    connection,
                    id,
                    ErrorCode::InternalError,
                    format!("internal error while handling {method}"),
                )
            }
        }
    }

    fn dispatch_notification_guarded(&mut self, method: String, params: serde_json::Value) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            let store = &self.store;
            self.index.drain_background(&|p| store.is_open_path(p));
            self.handle_notification(&method, params);
        })) && self.logged_panics.insert(method.clone())
        {
            let msg = panic_payload_message(&*payload);
            eprintln!("riscv-asm-lsp: panic while handling notification `{method}`: {msg}");
        }
    }

    fn handle_request(&mut self, req: Request) -> Result<(), Box<dyn std::error::Error>> {
        let id = req.id;
        let params = req.params;
        if req.method.starts_with("textDocument/")
            && let Some(uri_str) = params
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(|u| u.as_str())
            && let Ok(td_uri) = uri_str.parse::<Uri>()
            && (is_linker_script_uri(&td_uri) || self.is_unsupported_target_uri(&td_uri))
        {
            return reply_ok(self.connection, id, serde_json::Value::Null);
        }

        match req.method.as_str() {
            "textDocument/hover" => self.handle_hover(id, params),
            "textDocument/completion" => {
                self.dispatch_typed(id, params, |s, p: CompletionParams| {
                    let uri = &p.text_document_position.text_document.uri;
                    let pos = p.text_document_position.position;
                    Ok(s.store.get(uri).and_then(|doc| {
                        provide_completion(
                            doc,
                            pos,
                            s.db,
                            &mut s.ctx,
                            &mut s.index,
                            s.completion_caps,
                        )
                    }))
                })
            }
            "completionItem/resolve" => {
                self.dispatch_typed(id, params, |s, item: CompletionItem| {
                    Ok(resolve_completion_item(item, s.db))
                })
            }
            "textDocument/signatureHelp" => {
                self.dispatch_typed(id, params, |s, p: SignatureHelpParams| {
                    let uri = &p.text_document_position_params.text_document.uri;
                    let pos = p.text_document_position_params.position;
                    Ok(s.store
                        .get(uri)
                        .and_then(|doc| provide_signature_help(doc, pos, s.db, &mut s.ctx)))
                })
            }
            "textDocument/definition"
            | "textDocument/references"
            | "textDocument/documentHighlight"
            | "textDocument/prepareRename"
            | "textDocument/rename" => {
                self.handle_navigation_request(req.method.as_str(), id, params)
            }
            "textDocument/documentSymbol"
            | "workspace/symbol"
            | "textDocument/codeAction"
            | "textDocument/foldingRange"
            | "textDocument/inlayHint"
            | "textDocument/semanticTokens/full"
            | "textDocument/semanticTokens/range" => {
                self.handle_analysis_request(req.method.as_str(), id, params)
            }
            other => reply_err(
                self.connection,
                id,
                ErrorCode::MethodNotFound,
                format!("unsupported method: {other}"),
            ),
        }
    }

    fn handle_navigation_request(
        &mut self,
        method: &str,
        id: RequestId,
        params: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match method {
            "textDocument/definition" => {
                self.dispatch_typed(id, params, |s, p: GotoDefinitionParams| {
                    let uri = &p.text_document_position_params.text_document.uri;
                    let pos = p.text_document_position_params.position;
                    Ok(goto_definition(&mut s.index, uri, pos, &mut s.ctx))
                })
            }
            "textDocument/references" => {
                self.dispatch_typed(id, params, |s, p: ReferenceParams| {
                    let uri = &p.text_document_position.text_document.uri;
                    let pos = p.text_document_position.position;
                    Ok(find_references(
                        &mut s.index,
                        uri,
                        pos,
                        p.context.include_declaration,
                        &mut s.ctx,
                    ))
                })
            }
            "textDocument/documentHighlight" => {
                self.dispatch_typed(id, params, |s, p: DocumentHighlightParams| {
                    let uri = &p.text_document_position_params.text_document.uri;
                    let pos = p.text_document_position_params.position;
                    Ok(document_highlight(&mut s.index, uri, pos, &mut s.ctx))
                })
            }
            "textDocument/prepareRename" => {
                self.dispatch_typed(id, params, |s, p: TextDocumentPositionParams| {
                    Ok(prepare_rename(
                        &mut s.index,
                        &p.text_document.uri,
                        p.position,
                        s.db,
                        &mut s.ctx,
                    ))
                })
            }
            "textDocument/rename" => self.dispatch_typed(id, params, |s, p: RenameParams| {
                let uri = &p.text_document_position.text_document.uri;
                let pos = p.text_document_position.position;
                rename_symbol(&mut s.index, uri, pos, &p.new_name, s.db, &mut s.ctx)
            }),
            _ => unreachable!(),
        }
    }

    fn handle_analysis_request(
        &mut self,
        method: &str,
        id: RequestId,
        params: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match method {
            "textDocument/documentSymbol" => {
                self.dispatch_typed(id, params, |s, p: DocumentSymbolParams| {
                    Ok(document_symbols(&s.index, &p.text_document.uri))
                })
            }
            "workspace/symbol" => self.dispatch_typed(id, params, |s, p: WorkspaceSymbolParams| {
                Ok(workspace_symbols(&s.index, &p.query))
            }),
            "textDocument/codeAction" => {
                self.dispatch_typed(id, params, |s, p: CodeActionParams| {
                    let doc = s.store.get(&p.text_document.uri);
                    let preprocessed = s.ctx.for_uri(&p.text_document.uri).preprocessed;
                    Ok(provide_code_actions(
                        &p.text_document.uri,
                        p.range,
                        doc,
                        preprocessed,
                        &s.diag_worker.cache,
                    ))
                })
            }
            "textDocument/foldingRange" => {
                self.dispatch_typed(id, params, |s, p: FoldingRangeParams| {
                    let doc = s.store.get(&p.text_document.uri);
                    let preprocessed = s.ctx.for_uri(&p.text_document.uri).preprocessed;
                    Ok(doc.and_then(|d| provide_folding_ranges(d, &s.index, preprocessed)))
                })
            }
            "textDocument/inlayHint" => self.dispatch_typed(id, params, |s, p: InlayHintParams| {
                let uri = &p.text_document.uri;
                Ok(s.store.get(uri).and_then(|doc| {
                    provide_inlay_hints(doc, p.range, s.db, &mut s.ctx, &mut s.index)
                }))
            }),
            "textDocument/semanticTokens/full" => {
                self.dispatch_typed(id, params, |s, p: SemanticTokensParams| {
                    Ok(s.store
                        .get(&p.text_document.uri)
                        .and_then(|doc| {
                            provide_semantic_tokens_full(doc, s.db, &mut s.ctx, &mut s.index)
                        })
                        .map(SemanticTokensResult::Tokens))
                })
            }
            "textDocument/semanticTokens/range" => {
                self.dispatch_typed(id, params, |s, p: SemanticTokensRangeParams| {
                    Ok(s.store
                        .get(&p.text_document.uri)
                        .and_then(|doc| {
                            provide_semantic_tokens_range(
                                doc,
                                p.range,
                                s.db,
                                &mut s.ctx,
                                &mut s.index,
                            )
                        })
                        .map(SemanticTokensResult::Tokens))
                })
            }
            _ => unreachable!(),
        }
    }

    fn dispatch_typed<P, R, F>(
        &mut self,
        id: RequestId,
        params: serde_json::Value,
        handler: F,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        P: DeserializeOwned,
        R: Serialize,
        F: FnOnce(&mut Self, P) -> Result<R, String>,
    {
        let parsed: P = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(err) => {
                return reply_err(
                    self.connection,
                    id,
                    ErrorCode::InvalidParams,
                    err.to_string(),
                );
            }
        };
        match handler(self, parsed) {
            Ok(res) => reply_ok(self.connection, id, res),
            Err(msg) => reply_err(self.connection, id, ErrorCode::InvalidParams, msg),
        }
    }

    fn handle_hover(
        &mut self,
        id: RequestId,
        params: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let parsed: HoverParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(err) => {
                return reply_err(
                    self.connection,
                    id,
                    ErrorCode::InvalidParams,
                    err.to_string(),
                );
            }
        };
        let uri = &parsed.text_document_position_params.text_document.uri;
        let pos = parsed.text_document_position_params.position;
        let Some(doc) = self.store.get(uri) else {
            return reply_ok(self.connection, id, Option::<lsp_types::Hover>::None);
        };
        match prepare_hover(
            doc,
            pos,
            self.db,
            &mut self.ctx,
            &mut self.index,
            &self.tools,
            &self.hover_worker.cache,
        ) {
            HoverResolution::Ready(hover) => reply_ok(self.connection, id, hover),
            HoverResolution::Deferred(deferred) => {
                self.hover_worker.submit(id, *deferred);
                Ok(())
            }
        }
    }

    fn is_unsupported_target_uri(&mut self, uri: &Uri) -> bool {
        let file_ctx = self.ctx.for_uri(uri);
        if file_ctx.is_riscv {
            return false;
        }
        if self.logged_non_riscv.insert(file_ctx.target.clone()) {
            eprintln!(
                "riscv-asm-lsp: ignoring files with non-RISC-V target `{}`",
                file_ctx.target
            );
        }
        true
    }

    fn handle_notification(&mut self, method: &str, params: serde_json::Value) {
        match method {
            "$/cancelRequest" => {
                if let Some(id_val) = params.get("id") {
                    if let Some(n) = id_val.as_i64().and_then(|n| i32::try_from(n).ok()) {
                        self.hover_worker.cancel(&RequestId::from(n));
                    } else if let Some(s) = id_val.as_str() {
                        self.hover_worker.cancel(&RequestId::from(s.to_owned()));
                    }
                }
            }
            "textDocument/didOpen" => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) {
                    let uri = p.text_document.uri;
                    if is_linker_script_uri(&uri) || self.is_unsupported_target_uri(&uri) {
                        self.diag_worker.clear(&uri);
                        self.index.remove_path(&uri);
                        return;
                    }
                    let version = p.text_document.version;
                    let text = p.text_document.text;
                    self.dirty_docs.remove(&crate::context::uri_key(&uri));
                    let file_ctx = self.ctx.for_uri(&uri);
                    let doc = self.store.open(uri.clone(), version, text.clone());
                    self.index
                        .upsert_open_document_with_preprocessed(doc, file_ctx.preprocessed);
                    self.index.prioritize_open_document(&uri);
                    self.diag_worker
                        .schedule_immediate(uri, version, text, file_ctx);
                }
            }
            "textDocument/didChange" => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) {
                    let uri = p.text_document.uri;
                    if is_linker_script_uri(&uri) || self.is_unsupported_target_uri(&uri) {
                        return;
                    }
                    let version = p.text_document.version;
                    if let Some(doc) = self.store.change(&uri, version, p.content_changes) {
                        self.dirty_docs.insert(
                            crate::context::uri_key(&uri),
                            (uri.clone(), Instant::now() + INDEX_DEBOUNCE),
                        );
                        let text = doc.text.clone();
                        let file_ctx = self.ctx.for_uri(&uri);
                        self.diag_worker.schedule(uri, version, text, file_ctx);
                    }
                }
            }
            "textDocument/didSave" => {
                if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(params) {
                    let uri = p.text_document.uri;
                    if is_linker_script_uri(&uri) || self.is_unsupported_target_uri(&uri) {
                        return;
                    }
                    self.dirty_docs.remove(&crate::context::uri_key(&uri));
                    let is_header_or_inc = uri_to_path(&uri).is_some_and(|path| {
                        matches!(
                            path.extension().and_then(|e| e.to_str()),
                            Some("h" | "H" | "hpp" | "inc")
                        )
                    });
                    if is_header_or_inc {
                        self.hover_worker.clear_cache();
                        self.diag_worker.invalidate_all();
                    }
                    if let Some(doc) = self.store.get(&uri) {
                        let file_ctx = self.ctx.for_uri(&uri);
                        self.index
                            .upsert_open_document_with_preprocessed(doc, file_ctx.preprocessed);
                        let version = doc.version;
                        let text = doc.text.clone();
                        self.diag_worker
                            .schedule_immediate(uri, version, text, file_ctx);
                    } else {
                        self.hover_worker.clear_cache();
                        self.diag_worker.invalidate_all();
                        let pre = self.ctx.for_uri(&uri).preprocessed;
                        self.index.reload_path_with_preprocessed(&uri, Some(pre));
                    }
                }
            }
            "textDocument/didClose" => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(params) {
                    self.dirty_docs
                        .remove(&crate::context::uri_key(&p.text_document.uri));
                    self.store.close(&p.text_document.uri);
                    if !is_linker_script_uri(&p.text_document.uri)
                        && !self.is_unsupported_target_uri(&p.text_document.uri)
                    {
                        let pre = self.ctx.for_uri(&p.text_document.uri).preprocessed;
                        self.index
                            .reload_path_with_preprocessed(&p.text_document.uri, Some(pre));
                    }
                    self.diag_worker.clear(&p.text_document.uri);
                }
            }
            "workspace/didChangeWatchedFiles" => {
                if let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) {
                    self.handle_watched_files(p);
                }
            }
            "workspace/didChangeConfiguration" => {
                if let Ok(p) = serde_json::from_value::<DidChangeConfigurationParams>(params) {
                    self.handle_did_change_configuration(p);
                }
            }
            "workspace/didChangeWorkspaceFolders" => {
                if let Ok(p) = serde_json::from_value::<DidChangeWorkspaceFoldersParams>(params) {
                    self.handle_did_change_workspace_folders(p);
                }
            }
            _ => {}
        }
    }

    fn handle_did_change_configuration(&mut self, params: DidChangeConfigurationParams) {
        let settings_val = params
            .settings
            .get("riscv-asm-lsp")
            .or_else(|| params.settings.get("riscvAsmLsp"))
            .cloned()
            .unwrap_or(params.settings);
        let merged = merge_json_value(&self.init_options_raw, &settings_val);
        let new_cfg = Config::from_init_options(Some(merged));
        self.tools
            .update_bins(new_cfg.clang.clone(), new_cfg.llvm_mc.clone());
        self.diag_worker.update_config(new_cfg.diagnostics.clone());
        self.ctx.update_config(new_cfg);
        self.hover_worker.clear_cache();
        self.diag_worker.invalidate_all();
        self.refresh_open_docs_after_context_change();
    }

    fn handle_did_change_workspace_folders(&mut self, params: DidChangeWorkspaceFoldersParams) {
        let removed: HashSet<PathBuf> = params
            .event
            .removed
            .iter()
            .filter_map(|f| uri_to_path(&f.uri))
            .collect();
        let mut roots: Vec<PathBuf> = self
            .ctx
            .workspace_roots()
            .iter()
            .filter(|r| !removed.contains(*r))
            .cloned()
            .collect();
        for folder in params.event.added {
            if let Some(p) = uri_to_path(&folder.uri)
                && !roots.contains(&p)
            {
                roots.push(p);
            }
        }
        self.ctx.set_workspace_roots(roots.clone());
        let req_id = RequestId::from(self.next_req_id.fetch_add(1, Ordering::Relaxed));
        let store = &self.store;
        self.index.update_workspace_roots(
            roots,
            &|p| store.is_open_path(p),
            self.progress_ok.then(|| make_message_sink(self.connection)),
            req_id,
        );
        self.hover_worker.clear_cache();
        self.diag_worker.invalidate_all();
        self.refresh_open_docs_after_context_change();
    }

    fn refresh_open_docs_after_context_change(&mut self) {
        self.index.set_excluded_paths(self.ctx.non_riscv_paths());
        for (open_uri, version, text) in self.store.all_open_snapshots() {
            let file_ctx = self.ctx.for_uri(&open_uri);
            if !file_ctx.is_riscv {
                self.index.remove_path(&open_uri);
                self.diag_worker.clear(&open_uri);
            } else {
                self.index
                    .sync_preprocessed(&open_uri, file_ctx.preprocessed);
                self.diag_worker
                    .schedule_immediate(open_uri, version, text, file_ctx);
            }
        }
    }

    fn handle_watched_files(&mut self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            if is_linker_script_uri(&change.uri) {
                continue;
            }
            if change.uri.as_str().ends_with("compile_commands.json")
                || change.uri.as_str().ends_with("compile_flags.txt")
                || change.uri.as_str().ends_with(".riscv-asm-lsp.yaml")
                || change.uri.as_str().ends_with(".riscv-asm-lsp.yml")
            {
                self.ctx.reload_if_needed();
                self.hover_worker.clear_cache();
                self.diag_worker.invalidate_all();
                self.refresh_open_docs_after_context_change();
                continue;
            }
            self.hover_worker.clear_cache();
            self.diag_worker.invalidate_all();
            if self.store.get(&change.uri).is_some() {
                continue;
            }
            if change.typ == FileChangeType::DELETED {
                self.index.remove_path(&change.uri);
            } else {
                let pre = self.ctx.for_uri(&change.uri).preprocessed;
                self.index
                    .reload_path_with_preprocessed(&change.uri, Some(pre));
            }
        }
    }

    fn flush_due_dirty_docs(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, Uri)> = self
            .dirty_docs
            .iter()
            .filter(|(_, (_, due_at))| *due_at <= now)
            .map(|(k, (u, _))| (k.clone(), u.clone()))
            .collect();
        for (key, uri) in due {
            self.dirty_docs.remove(&key);
            if let Some(doc) = self.store.get(&uri) {
                let pre = self.ctx.for_uri(&uri).preprocessed;
                self.index.upsert_open_document_with_preprocessed(doc, pre);
            }
        }
    }

    fn flush_all_dirty_docs(&mut self) {
        for (_, (uri, _)) in self.dirty_docs.drain() {
            if let Some(doc) = self.store.get(&uri) {
                let pre = self.ctx.for_uri(&uri).preprocessed;
                self.index.upsert_open_document_with_preprocessed(doc, pre);
            }
        }
    }
}

fn make_message_sink(connection: &Connection) -> MessageSink {
    let sender = connection.sender.clone();
    Arc::new(move |msg| {
        let _ = sender.send(msg);
    })
}

fn register_watched_files_if_supported(
    connection: &Connection,
    init_params: &InitializeParams,
    alloc_req_id: &impl Fn() -> RequestId,
) {
    let dynamic_ok = init_params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.did_change_watched_files)
        .and_then(|c| c.dynamic_registration)
        == Some(true);
    if !dynamic_ok {
        return;
    }

    let reg_options = DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.{S,s,sx,h,inc}".to_owned()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/compile_commands.json".to_owned()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/compile_flags.txt".to_owned()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/.riscv-asm-lsp.{yaml,yml}".to_owned()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
        ],
    };
    let Ok(register_options) = serde_json::to_value(reg_options) else {
        return;
    };
    let params = RegistrationParams {
        registrations: vec![Registration {
            id: "riscv-asm-lsp-watched-files".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: Some(register_options),
        }],
    };
    if let Ok(params_val) = serde_json::to_value(params) {
        let req = Request::new(
            alloc_req_id(),
            "client/registerCapability".to_owned(),
            params_val,
        );
        let _ = connection.sender.send(Message::Request(req));
    }
}

fn extract_completion_caps(params: &InitializeParams) -> CompletionClientCaps {
    let comp_item = params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|td| td.completion.as_ref())
        .and_then(|c| c.completion_item.as_ref());
    let resolve_documentation_support = comp_item
        .and_then(|ci| ci.resolve_support.as_ref())
        .is_some_and(|rs| rs.properties.iter().any(|p| p == "documentation"));
    CompletionClientCaps {
        snippet_support: comp_item.and_then(|ci| ci.snippet_support) == Some(true),
        label_details_support: comp_item.and_then(|ci| ci.label_details_support) == Some(true),
        resolve_documentation_support,
    }
}

// Legacy clients may still send deprecated `root_uri` or `root_path` instead of `workspace_folders`.
#[allow(deprecated)]
fn extract_workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        let roots: Vec<PathBuf> = folders.iter().filter_map(|f| uri_to_path(&f.uri)).collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    if let Some(root_uri) = &params.root_uri
        && let Some(path) = uri_to_path(root_uri)
    {
        return vec![path];
    }
    params
        .root_path
        .as_deref()
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

pub(crate) fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

fn reply_ok<T: Serialize>(
    connection: &Connection,
    id: RequestId,
    value: T,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = serde_json::to_value(value)?;
    connection
        .sender
        .send(Message::Response(Response::new_ok(id, result)))?;
    Ok(())
}

fn reply_err(
    connection: &Connection,
    id: RequestId,
    code: ErrorCode,
    message: String,
) -> Result<(), Box<dyn std::error::Error>> {
    connection.sender.send(Message::Response(Response::new_err(
        id,
        code as i32,
        message,
    )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_guarded_request_test<F>(
        connection: &Connection,
        id: RequestId,
        method: &str,
        logged_panics: &mut HashSet<String>,
        f: F,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
    {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(res) => res,
            Err(payload) => {
                if logged_panics.insert(method.to_owned()) {
                    let msg = panic_payload_message(&*payload);
                    eprintln!("riscv-asm-lsp: panic while handling request `{method}`: {msg}");
                }
                reply_err(
                    connection,
                    id,
                    ErrorCode::InternalError,
                    format!("internal error while handling {method}"),
                )
            }
        }
    }

    #[test]
    fn panic_guard_replies_internal_error_and_continues() {
        let (server_conn, client_conn) = Connection::memory();
        let mut logged = HashSet::new();

        let res = run_guarded_request_test(
            &server_conn,
            RequestId::from(42),
            "textDocument/hover",
            &mut logged,
            || panic!("simulated request panic"),
        );
        assert!(res.is_ok());
        assert!(logged.contains("textDocument/hover"));

        let msg = client_conn.receiver.recv().expect("error response");
        let Message::Response(resp) = msg else {
            panic!("expected response");
        };
        assert_eq!(resp.id, RequestId::from(42));
        let err = resp.response_result.expect_err("expected InternalError");
        assert_eq!(err.code, ErrorCode::InternalError as i32);
    }
}
