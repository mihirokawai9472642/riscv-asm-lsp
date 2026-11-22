use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lsp_types::{InitializeResult, Uri};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct IncomingNotification {
    pub method: String,
    #[allow(dead_code)]
    pub params: Value,
}

#[derive(Debug, Clone)]
pub struct IncomingResponse {
    pub id: i64,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

enum RawMessage {
    Response(IncomingResponse),
    Notification(IncomingNotification),
    Eof,
}

pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<RawMessage>,
    next_id: i64,
    pub notifications: VecDeque<IncomingNotification>,
    pub init_result: InitializeResult,
}

impl LspClient {
    pub fn start(init_options: Option<Value>, root_uri: Option<&Uri>) -> Self {
        Self::start_with_env(init_options, root_uri, &[])
    }

    pub fn start_with_env(
        init_options: Option<Value>,
        root_uri: Option<&Uri>,
        envs: &[(&str, &str)],
    ) -> Self {
        Self::start_with_options(init_options, root_uri, envs, false, false, false, false)
    }

    #[allow(dead_code)]
    pub fn start_with_watched_files(init_options: Option<Value>, root_uri: Option<&Uri>) -> Self {
        Self::start_with_options(init_options, root_uri, &[], true, false, false, false)
    }

    #[allow(dead_code)]
    pub fn start_with_snippets(init_options: Option<Value>, root_uri: Option<&Uri>) -> Self {
        Self::start_with_options(init_options, root_uri, &[], false, true, true, false)
    }

    #[allow(dead_code)]
    pub fn start_with_work_done_progress(
        init_options: Option<Value>,
        root_uri: Option<&Uri>,
    ) -> Self {
        Self::start_with_options(init_options, root_uri, &[], false, false, false, true)
    }

    fn start_with_options(
        init_options: Option<Value>,
        root_uri: Option<&Uri>,
        envs: &[(&str, &str)],
        watched_files_dynamic: bool,
        snippet_support: bool,
        label_details_support: bool,
        work_done_progress: bool,
    ) -> Self {
        let bin = env!("CARGO_BIN_EXE_riscv-asm-lsp");
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().expect("failed to spawn riscv-asm-lsp");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain stderr so the child never blocks on stderr pipe buffer.
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                line.clear();
            }
        });

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_lsp_message(&mut reader) {
                let is_response = msg.get("id").is_some() && msg.get("method").is_none();
                let parsed = if is_response {
                    let id = msg.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
                    RawMessage::Response(IncomingResponse {
                        id,
                        result: msg.get("result").cloned(),
                        error: msg.get("error").cloned(),
                    })
                } else {
                    let method = msg
                        .get("method")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    RawMessage::Notification(IncomingNotification { method, params })
                };
                if tx.send(parsed).is_err() {
                    break;
                }
            }
            let _ = tx.send(RawMessage::Eof);
        });

        let mut client = Self {
            child,
            stdin,
            rx,
            next_id: 1,
            notifications: VecDeque::new(),
            init_result: InitializeResult::default(),
        };

        let mut capabilities = json!({
            "textDocument": {
                "synchronization": {
                    "dynamicRegistration": false,
                    "willSave": false,
                    "willSaveWaitUntil": false,
                    "didSave": true
                },
                "hover": {
                    "contentFormat": ["markdown", "plaintext"]
                },
                "completion": {
                    "completionItem": {
                        "snippetSupport": snippet_support,
                        "labelDetailsSupport": label_details_support,
                        "documentationFormat": ["markdown", "plaintext"]
                    }
                },
                "documentSymbol": {
                    "hierarchicalDocumentSymbolSupport": true
                }
            }
        });
        if watched_files_dynamic {
            capabilities["workspace"] = json!({
                "didChangeWatchedFiles": {
                    "dynamicRegistration": true
                }
            });
        }
        if work_done_progress {
            capabilities["window"] = json!({
                "workDoneProgress": true
            });
        }
        let mut params = json!({
            "processId": std::process::id(),
            "capabilities": capabilities
        });
        if let Some(uri) = root_uri {
            params["rootUri"] = json!(uri.as_str());
            params["workspaceFolders"] = json!([{
                "uri": uri.as_str(),
                "name": "workspace"
            }]);
        } else {
            params["rootUri"] = Value::Null;
        }
        if let Some(opts) = init_options {
            params["initializationOptions"] = opts;
        }

        let resp = client.send_request("initialize", params);
        assert!(resp.error.is_none(), "initialize error: {:?}", resp.error);
        let init_val = resp.result.expect("initialize result missing");
        client.init_result = serde_json::from_value(init_val).expect("valid InitializeResult");
        client.send_notification("initialized", json!({}));
        client
    }

    pub fn send_request(&mut self, method: &str, params: Value) -> IncomingResponse {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        write_lsp_message(&mut self.stdin, &payload);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for response to {method} (id={id})"
            );
            match self.rx.recv_timeout(remaining) {
                Ok(RawMessage::Response(resp)) if resp.id == id => return resp,
                Ok(RawMessage::Response(other)) => {
                    panic!(
                        "unexpected response id={} while waiting for id={id}",
                        other.id
                    );
                }
                Ok(RawMessage::Notification(not)) => {
                    self.notifications.push_back(not);
                }
                Ok(RawMessage::Eof) => {
                    panic!("server exited unexpectedly while waiting for {method}");
                }
                Err(_) => panic!("timed out waiting for {method}"),
            }
        }
    }

    pub fn send_notification(&mut self, method: &str, params: Value) {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        write_lsp_message(&mut self.stdin, &payload);
    }

    pub fn open_doc(&mut self, uri: &Uri, version: i32, text: &str) {
        self.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri.as_str(),
                    "languageId": "riscv-assembly",
                    "version": version,
                    "text": text
                }
            }),
        );
    }

    pub fn change_doc_incremental(
        &mut self,
        uri: &Uri,
        version: i32,
        range: lsp_types::Range,
        text: &str,
    ) {
        self.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": uri.as_str(),
                    "version": version
                },
                "contentChanges": [{
                    "range": {
                        "start": { "line": range.start.line, "character": range.start.character },
                        "end": { "line": range.end.line, "character": range.end.character }
                    },
                    "text": text
                }]
            }),
        );
    }

    #[allow(dead_code)]
    pub fn wait_for_notification(
        &mut self,
        method: &str,
        timeout: Duration,
    ) -> Option<IncomingNotification> {
        if let Some(pos) = self.notifications.iter().position(|n| n.method == method) {
            return self.notifications.remove(pos);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(RawMessage::Notification(n)) if n.method == method => return Some(n),
                Ok(RawMessage::Notification(n)) => self.notifications.push_back(n),
                Ok(RawMessage::Response(_)) | Ok(RawMessage::Eof) | Err(_) => return None,
            }
        }
    }

    pub fn shutdown_and_exit(mut self) {
        let resp = self.send_request("shutdown", Value::Null);
        assert!(resp.error.is_none(), "shutdown failed: {:?}", resp.error);
        self.send_notification("exit", Value::Null);
        let status = self.child.wait().expect("wait failed");
        assert!(
            status.success(),
            "server exited with non-zero status: {status}"
        );
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("tests")
        .join("fixtures")
}

pub fn path_to_uri(path: &std::path::Path) -> Uri {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = format!("file://{}", abs.display());
    s.parse().expect("valid file URI")
}

#[allow(dead_code)]
pub fn has_tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn completion_items_vec(result: &Option<Value>) -> Vec<Value> {
    let Some(val) = result.as_ref() else {
        return Vec::new();
    };
    val.as_array()
        .or_else(|| val.get("items").and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn completion_labels(result: &Option<Value>) -> Vec<String> {
    completion_items_vec(result)
        .into_iter()
        .filter_map(|x| x.get("label").and_then(|l| l.as_str()).map(str::to_owned))
        .collect()
}

#[allow(dead_code)]
pub fn hover_markdown(
    client: &mut LspClient,
    uri: &Uri,
    line: u32,
    character: u32,
) -> Option<String> {
    let resp = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": line, "character": character }
        }),
    );
    assert!(resp.error.is_none(), "hover error: {:?}", resp.error);
    resp.result.and_then(|v| {
        v.get("contents")
            .and_then(|c| c.get("value"))
            .and_then(|s| s.as_str())
            .map(str::to_owned)
    })
}

#[allow(dead_code)]
pub fn workspace_symbol_names(client: &mut LspClient, query: &str) -> Vec<String> {
    let resp = client.send_request("workspace/symbol", json!({ "query": query }));
    assert!(
        resp.error.is_none(),
        "workspace/symbol error: {:?}",
        resp.error
    );
    resp.result
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect()
}

fn write_lsp_message(stdin: &mut ChildStdin, payload: &Value) {
    let body = serde_json::to_vec(payload).expect("serialize JSON-RPC");
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).expect("write header");
    stdin.write_all(&body).expect("write body");
    stdin.flush().expect("flush");
}

fn read_lsp_message<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, val)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            content_length = val.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}
