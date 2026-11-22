use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lsp_server::{Connection, ErrorCode, Message, Request, RequestId, Response};
use lsp_types::{
    CodeActionParams, CodeActionProviderCapability, CompletionOptions, CompletionParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentHighlightParams,
    DocumentSymbolParams, FileChangeType, FileSystemWatcher, GlobPattern, GotoDefinitionParams,
    HoverParams, HoverProviderCapability, InitializeParams, OneOf, ReferenceParams, Registration,
    RegistrationParams, RenameOptions, RenameParams, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensRangeParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, WatchKind, WorkspaceSymbolParams,
};
use serde::Serialize;

use crate::code_action::provide_code_actions;
use crate::completion::{CompletionClientCaps, provide_completion};
use crate::config::Config;
use crate::context::{CompileContext, uri_to_path};
use crate::data::Database;
use crate::diagnostics::{DiagnosticsCache, DiagnosticsWorker, MessageSink};
use crate::document::DocumentStore;
use crate::hover::{HoverResolution, HoverWorker, prepare_hover};
use crate::index::WorkspaceIndex;
use crate::navigation::{
    document_highlight, find_references, goto_definition, prepare_rename, rename_symbol,
};
use crate::semantic_tokens::{legend, provide_semantic_tokens_full, provide_semantic_tokens_range};
use crate::symbols::{document_symbols, workspace_symbols};
use crate::tools::ToolRunner;

pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
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
        ..ServerCapabilities::default()
    }
}

pub fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::load()?;
    let (connection, io_threads) = Connection::stdio();

    let caps_json = serde_json::to_value(server_capabilities())?;
    let init_params_json = connection.initialize(caps_json)?;
    let init_params: InitializeParams = serde_json::from_value(init_params_json)?;

    register_watched_files_if_supported(&connection, &init_params);

    let completion_caps = extract_completion_caps(&init_params);
    let progress_ok = init_params
        .capabilities
        .window
        .as_ref()
        .and_then(|w| w.work_done_progress)
        == Some(true);
    let workspace_root = extract_workspace_root(&init_params);
    let config = Config::from_init_options(init_params.initialization_options);
    let mut ctx = CompileContext::new(workspace_root.clone(), config.clone());
    let mut store = DocumentStore::new().map_err(std::io::Error::other)?;
    let mut index = WorkspaceIndex::new(workspace_root).map_err(std::io::Error::other)?;
    index.start_background_scan(progress_ok.then(|| make_message_sink(&connection)));
    let tools = Arc::new(ToolRunner::new(
        config.clang.clone(),
        config.llvm_mc.clone(),
    ));
    let diag_sender = make_message_sink(&connection);
    let diag_worker = DiagnosticsWorker::new(
        config.diagnostics.enable,
        config.diagnostics.delay_ms,
        Arc::clone(&tools),
        diag_sender,
    );
    let hover_worker = HoverWorker::new(Arc::clone(&tools), make_message_sink(&connection));

    eprintln!(
        "riscv-asm-lsp started (default_target={}, default_march={}, instructions={}, csrs={})",
        config.default_target,
        config.default_march,
        db.raw.instructions.len(),
        db.raw.csrs.len()
    );

    let loop_res = main_loop(
        &connection,
        &db,
        &mut ctx,
        &mut store,
        &mut index,
        &tools,
        &diag_worker,
        &hover_worker,
        completion_caps,
    );
    // Shut down background workers before dropping `connection` so `LspServerWriter` drains cleanly.
    index.shutdown();
    diag_worker.shutdown();
    hover_worker.shutdown();
    drop(connection);
    io_threads.join()?;
    loop_res
}

fn make_message_sink(connection: &Connection) -> MessageSink {
    let sender = connection.sender.clone();
    Arc::new(move |msg| {
        let _ = sender.send(msg);
    })
}

fn register_watched_files_if_supported(connection: &Connection, init_params: &InitializeParams) {
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
                glob_pattern: GlobPattern::String("**/*.{S,s,h,inc}".to_owned()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/compile_commands.json".to_owned()),
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
            RequestId::from(1),
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
    CompletionClientCaps {
        snippet_support: comp_item.and_then(|ci| ci.snippet_support) == Some(true),
        label_details_support: comp_item.and_then(|ci| ci.label_details_support) == Some(true),
    }
}

#[allow(deprecated)]
fn extract_workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folders) = &params.workspace_folders
        && let Some(first) = folders.first()
        && let Some(path) = uri_to_path(&first.uri)
    {
        return Some(path);
    }
    if let Some(root_uri) = &params.root_uri
        && let Some(path) = uri_to_path(root_uri)
    {
        return Some(path);
    }
    params.root_path.as_deref().map(PathBuf::from)
}

#[allow(clippy::too_many_arguments)]
fn main_loop(
    connection: &Connection,
    db: &Database,
    ctx: &mut CompileContext,
    store: &mut DocumentStore,
    index: &mut WorkspaceIndex,
    tools: &ToolRunner,
    diag_worker: &DiagnosticsWorker,
    hover_worker: &HoverWorker,
    completion_caps: CompletionClientCaps,
) -> Result<(), Box<dyn std::error::Error>> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                if matches!(
                    req.method.as_str(),
                    "textDocument/definition"
                        | "textDocument/references"
                        | "textDocument/rename"
                        | "workspace/symbol"
                ) {
                    index.drain_background_wait(
                        &|u| store.get(u).is_some(),
                        Duration::from_millis(200),
                    );
                } else {
                    index.drain_background(&|u| store.get(u).is_some());
                }
                handle_request(
                    connection,
                    req,
                    db,
                    ctx,
                    store,
                    index,
                    tools,
                    &diag_worker.cache,
                    hover_worker,
                    completion_caps,
                )?;
            }
            Message::Notification(not) => {
                index.drain_background(&|u| store.get(u).is_some());
                handle_notification(&not.method, not.params, ctx, store, index, diag_worker);
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    connection: &Connection,
    req: Request,
    db: &Database,
    ctx: &mut CompileContext,
    store: &DocumentStore,
    index: &mut WorkspaceIndex,
    tools: &ToolRunner,
    diag_cache: &DiagnosticsCache,
    hover_worker: &HoverWorker,
    completion_caps: CompletionClientCaps,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = req.id.clone();
    match req.method.as_str() {
        "textDocument/hover" => {
            let params: HoverParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let Some(doc) = store.get(uri) else {
                return reply_ok(connection, id, Option::<lsp_types::Hover>::None);
            };
            match prepare_hover(doc, pos, db, ctx, index, tools, &hover_worker.cache) {
                HoverResolution::Ready(hover) => reply_ok(connection, id, hover)?,
                HoverResolution::Deferred(deferred) => hover_worker.submit(id, *deferred),
            }
        }
        "textDocument/completion" => {
            let params: CompletionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let result = store
                .get(uri)
                .and_then(|doc| provide_completion(doc, pos, db, ctx, index, completion_caps));
            reply_ok(connection, id, result)?;
        }
        "textDocument/definition" => {
            let params: GotoDefinitionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let result = goto_definition(index, uri, pos, ctx);
            reply_ok(connection, id, result)?;
        }
        "textDocument/references" => {
            let params: ReferenceParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let include_decl = params.context.include_declaration;
            let result = find_references(index, uri, pos, include_decl, ctx);
            reply_ok(connection, id, result)?;
        }
        "textDocument/documentHighlight" => {
            let params: DocumentHighlightParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let result = document_highlight(index, uri, pos, ctx);
            reply_ok(connection, id, result)?;
        }
        "textDocument/prepareRename" => {
            let params: TextDocumentPositionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = prepare_rename(index, &params.text_document.uri, params.position, db, ctx);
            reply_ok(connection, id, result)?;
        }
        "textDocument/rename" => {
            let params: RenameParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let uri = &params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let result = rename_symbol(index, uri, pos, &params.new_name, db, ctx);
            reply_ok(connection, id, result)?;
        }
        "textDocument/documentSymbol" => {
            let params: DocumentSymbolParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = document_symbols(index, &params.text_document.uri);
            reply_ok(connection, id, result)?;
        }
        "workspace/symbol" => {
            let params: WorkspaceSymbolParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = workspace_symbols(index, &params.query);
            reply_ok(connection, id, result)?;
        }
        "textDocument/codeAction" => {
            let params: CodeActionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = provide_code_actions(&params.text_document.uri, params.range, diag_cache);
            reply_ok(connection, id, result)?;
        }
        "textDocument/semanticTokens/full" => {
            let params: SemanticTokensParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = store
                .get(&params.text_document.uri)
                .and_then(|doc| provide_semantic_tokens_full(doc, db, ctx, index))
                .map(SemanticTokensResult::Tokens);
            reply_ok(connection, id, result)?;
        }
        "textDocument/semanticTokens/range" => {
            let params: SemanticTokensRangeParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(err) => {
                    return reply_err(connection, id, ErrorCode::InvalidParams, err.to_string());
                }
            };
            let result = store
                .get(&params.text_document.uri)
                .and_then(|doc| provide_semantic_tokens_range(doc, params.range, db, ctx, index))
                .map(SemanticTokensResult::Tokens);
            reply_ok(connection, id, result)?;
        }
        other => {
            reply_err(
                connection,
                id,
                ErrorCode::MethodNotFound,
                format!("unsupported method: {other}"),
            )?;
        }
    }
    Ok(())
}

fn handle_notification(
    method: &str,
    params: serde_json::Value,
    ctx: &mut CompileContext,
    store: &mut DocumentStore,
    index: &mut WorkspaceIndex,
    diag_worker: &DiagnosticsWorker,
) {
    match method {
        "textDocument/didOpen" => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(params) {
                let uri = p.text_document.uri;
                let version = p.text_document.version;
                let text = p.text_document.text;
                let doc = store.open(uri.clone(), version, text.clone());
                index.upsert_open_document(doc);
                let file_ctx = ctx.for_uri(&uri);
                diag_worker.schedule(uri, version, text, file_ctx);
            }
        }
        "textDocument/didChange" => {
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(params) {
                let uri = p.text_document.uri;
                let version = p.text_document.version;
                if let Some(doc) = store.change(&uri, version, p.content_changes) {
                    index.upsert_open_document(doc);
                    let text = doc.text.clone();
                    let file_ctx = ctx.for_uri(&uri);
                    diag_worker.schedule(uri, version, text, file_ctx);
                }
            }
        }
        "textDocument/didSave" => {
            if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(params) {
                let uri = p.text_document.uri;
                if let Some(doc) = store.get(&uri) {
                    index.upsert_open_document(doc);
                    let version = doc.version;
                    let text = doc.text.clone();
                    let file_ctx = ctx.for_uri(&uri);
                    diag_worker.schedule(uri, version, text, file_ctx);
                } else {
                    index.reload_path(&uri);
                }
            }
        }
        "textDocument/didClose" => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(params) {
                store.close(&p.text_document.uri);
                index.reload_path(&p.text_document.uri);
                diag_worker.clear(&p.text_document.uri);
            }
        }
        "workspace/didChangeWatchedFiles" => {
            if let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) {
                for change in p.changes {
                    if change.uri.as_str().ends_with("compile_commands.json") {
                        ctx.reload_if_needed();
                        continue;
                    }
                    // Open buffers stay authoritative until saved/closed
                    if store.get(&change.uri).is_some() {
                        continue;
                    }
                    if change.typ == FileChangeType::DELETED {
                        index.remove_path(&change.uri);
                    } else {
                        index.reload_path(&change.uri);
                    }
                }
            }
        }
        _ => {}
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
