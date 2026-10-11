//! A minimal, purpose-built LSP client: one server process per workspace root.
//!
//! Spawns a language server over stdio, performs the `initialize` handshake,
//! and exposes the few requests/notifications the Coding Agent tools need:
//! open/change files, fetch a document symbol outline, and collect pushed
//! diagnostics. A background reader task dispatches frames: responses resolve
//! the matching request's oneshot channel; notifications update state
//! (publishDiagnostics) or are logged.
//!
//! Sized for the two LSP tools (`lsp_symbols` / `lsp_diagnostics`), not a
//! general-purpose client — deliberately no workspace/indexing orchestration.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dashmap::DashMap;
use lsp_types::{
    ClientCapabilities, CodeActionClientCapabilities, DidOpenTextDocumentParams,
    DocumentSymbolClientCapabilities, InitializeParams, PartialResultParams,
    RenameClientCapabilities, TextDocumentClientCapabilities, TextDocumentItem,
    WorkDoneProgressParams, WorkspaceClientCapabilities, WorkspaceFolder,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex, RwLock};

use super::transport::{read_frame, write_frame};

/// One in-flight request: the reader resolves this sender when a matching id arrives.
type Pending = oneshot::Sender<Result<Value>>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct LspClient {
    /// Write half of the server's stdin (locked for frame atomicity).
    stdin: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    /// id → pending response channel, owned by the reader task.
    pending: DashMap<i64, Pending>,
    /// uri string → last pushed diagnostics (per publishDiagnostics).
    diagnostics: RwLock<HashMap<String, Vec<lsp_types::Diagnostic>>>,
    /// Files we've already didOpen'd this session (didChange thereafter).
    open_files: Mutex<HashMap<String, i32>>,
    next_id: AtomicI64,
    request_timeout_ms: AtomicU64,
    /// Set when the reader task exits (server EOF/crash): requests issued
    /// afterwards fail immediately instead of riding the full timeout — the
    /// drain at reader exit only covers pendings that already existed.
    reader_dead: AtomicBool,
    root_uri: lsp_types::Uri,
    /// LSP language id sent with `textDocument/didOpen` ("rust",
    /// "typescript", "python", ...), owned by the spawning server spec.
    language_id: String,
    /// Capabilities the server returned in `initialize` — drives dynamic
    /// gating of optional requests (rename/codeAction/workspaceSymbol).
    server_capabilities: RwLock<Value>,
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Start the reader task and perform the initialize handshake.
    ///
    /// `child` is optional so tests can drive the client over in-memory duplex
    /// streams instead of a real server process.
    pub async fn spawn(
        root_dir: &Path,
        language_id: &str,
        stdin: Box<dyn AsyncWrite + Send + Unpin>,
        stdout: Box<dyn AsyncRead + Send + Unpin>,
        child: Option<Child>,
    ) -> Result<Arc<Self>> {
        let root_uri = super::file_uri(root_dir);
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending: DashMap::new(),
            diagnostics: RwLock::new(HashMap::new()),
            open_files: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            request_timeout_ms: AtomicU64::new(REQUEST_TIMEOUT.as_millis() as u64),
            reader_dead: AtomicBool::new(false),
            root_uri,
            language_id: language_id.to_string(),
            server_capabilities: RwLock::new(Value::Null),
            child: Mutex::new(child),
        });
        let reader = client.clone();
        tokio::spawn(async move { reader.reader_loop(stdout).await });
        if let Err(error) = client.initialize().await {
            // The reader task holds another Arc, so dropping ours does not
            // drop the Child — kill it explicitly or a silent binary leaks a
            // process per failed handshake.
            if let Some(mut child) = client.child.lock().await.take() {
                let _ = child.kill().await;
            }
            return Err(error);
        }
        Ok(client)
    }

    /// Spawn the server binary on PATH for `root_dir` and return its stdio
    /// halves. Binary override: `STARRIVER_LSP_BIN_<ID>` (e.g.
    /// `STARRIVER_LSP_BIN_RUST`), with the legacy
    /// `STARRIVER_RUST_ANALYZER` still honored for the Rust server.
    pub async fn spawn_process(
        root_dir: &Path,
        spec: &super::ServerSpec,
    ) -> Result<(
        Box<dyn AsyncWrite + Send + Unpin>,
        Box<dyn AsyncRead + Send + Unpin>,
        Child,
    )> {
        use std::process::Stdio;
        let generic_override = format!("STARRIVER_LSP_BIN_{}", spec.id.to_ascii_uppercase());
        let binary = std::env::var(&generic_override)
            .ok()
            .or_else(|| {
                if spec.id == "rust" {
                    std::env::var("STARRIVER_RUST_ANALYZER").ok()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| spec.command.clone());
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(root_dir)
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| {
            format!("spawn {binary:?} (is it installed? {})", spec.install_hint)
        })?;
        let stdin: Box<dyn AsyncWrite + Send + Unpin> =
            Box::new(child.stdin.take().context("no stdin")?);
        let stdout: Box<dyn AsyncRead + Send + Unpin> =
            Box::new(child.stdout.take().context("no stdout")?);
        if let Some(stderr) = child.stderr.take() {
            let server_id = spec.id.clone();
            tokio::spawn(async move {
                let mut r = tokio::io::BufReader::new(stderr);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match r.read_line(&mut buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let line = buf.trim_end();
                            if !line.is_empty() {
                                tracing::debug!(stderr = %line, server = %server_id);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Ok((stdin, stdout, child))
    }

    async fn reader_loop(&self, stdout: Box<dyn AsyncRead + Send + Unpin>) {
        let mut reader = tokio::io::BufReader::new(stdout);
        loop {
            match read_frame(&mut reader).await {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(v) => {
                        if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                            if let Some((_, tx)) = self.pending.remove(&id) {
                                let resp = if let Some(e) = v.get("error") {
                                    Err(anyhow::anyhow!("LSP error {e}"))
                                } else {
                                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                                };
                                let _ = tx.send(resp);
                                continue;
                            }
                        }
                        self.handle_frame(&v).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "lsp: unparseable frame"),
                },
                Err(e) => {
                    // Server exited or pipe closed: unblock every waiter so no
                    // tool call hangs forever. (DashMap has no drain in v6 —
                    // snapshot ids, then remove each.) The flag goes up BEFORE
                    // the drain so requests racing in after it are failed by
                    // request() itself instead of riding the timeout.
                    self.reader_dead.store(true, Ordering::SeqCst);
                    let ids: Vec<i64> = self.pending.iter().map(|e| *e.key()).collect();
                    for id in ids {
                        if let Some((_, tx)) = self.pending.remove(&id) {
                            let _ = tx.send(Err(anyhow::anyhow!("LSP server closed: {e}")));
                        }
                    }
                    break;
                }
            }
        }
        self.reader_dead.store(true, Ordering::SeqCst);
    }

    async fn handle_frame(&self, v: &Value) {
        // A frame with an id AND a method is a server→client request (we never
        // register handlers). Answer MethodNotFound so the server doesn't
        // block. Late/unmatched RESPONSES carry an id but no method — replying
        // to a response is meaningless and only confuses the server.
        if v.get("id").is_some() && v.get("method").is_some() {
            let _ = self
                .notify_raw(json!({
                    "jsonrpc": "2.0",
                    "id": v["id"],
                    "error": { "code": -32601, "message": "method not implemented" },
                }))
                .await;
            return;
        }
        let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
            return;
        };
        match method {
            "textDocument/publishDiagnostics" => {
                if let Ok(p) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(
                    v["params"].clone(),
                ) {
                    let uri = p.uri.as_str().to_string();
                    self.diagnostics.write().await.insert(uri, p.diagnostics);
                }
            }
            "window/logMessage" | "window/showMessage" => {
                if let Some(msg) = v["params"]["message"].as_str() {
                    tracing::debug!(msg, "lsp: server message");
                }
            }
            _ => tracing::trace!(method, "lsp: unhandled notification"),
        }
    }

    async fn notify_raw(&self, v: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        write_frame(&mut *stdin, &v).await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.notify_raw(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Send a request and wait for its response (pumping unrelated frames inline).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.notify_raw(frame).await {
            self.pending.remove(&id);
            return Err(e);
        }
        if self.reader_dead.load(Ordering::SeqCst) {
            self.pending.remove(&id);
            return Err(anyhow::anyhow!("LSP server closed: {method} not sent"));
        }
        let timeout = Duration::from_millis(self.request_timeout_ms.load(Ordering::SeqCst));
        let resp = match tokio::time::timeout(timeout, rx).await {
            Ok(r) => r.map_err(|_| anyhow::anyhow!("LSP channel closed"))?,
            Err(_) => {
                // Remove the waiter before returning — a hung server would
                // otherwise leak one pending entry per timed-out request.
                self.pending.remove(&id);
                let _ = self
                    .notify("$/cancelRequest", json!({ "id": id }))
                    .await;
                return Err(anyhow::anyhow!("LSP request timeout: {method}"));
            }
        };
        self.pending.remove(&id);
        resp
    }

    async fn initialize(&self) -> Result<()> {
        let folder_name = self
            .root_uri
            .as_str()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("workspace")
            .to_string();
        let init = InitializeParams {
            process_id: Some(std::process::id()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: self.root_uri.clone(),
                name: folder_name,
            }]),
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    // We apply WorkspaceEdits ourselves (rename/codeAction),
                    // the editor-side half of `workspace/applyEdit`.
                    apply_edit: Some(true),
                    ..Default::default()
                }),
                text_document: Some(TextDocumentClientCapabilities {
                    document_symbol: Some(DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    rename: Some(RenameClientCapabilities::default()),
                    code_action: Some(CodeActionClientCapabilities::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let resp = self
            .request("initialize", serde_json::to_value(init)?)
            .await?;
        // Keep the server's capabilities: optional requests (rename,
        // codeAction, workspaceSymbol) are gated on them at the tool layer.
        *self.server_capabilities.write().await =
            resp.get("capabilities").cloned().unwrap_or(Value::Null);
        self.notify("initialized", json!({})).await?;
        tracing::debug!(server_info = ?resp.get("serverInfo"), "lsp: initialized");
        Ok(())
    }

    /// Whether the server advertised `capability` in its initialize response.
    /// Both `true` and an object form (e.g. `renameProvider:
    /// {"prepareProvider": true}`) count as supported.
    pub async fn supports(&self, capability: &str) -> bool {
        let caps = self.server_capabilities.read().await;
        matches!(
            caps.get(capability),
            Some(Value::Bool(true)) | Some(Value::Object(_))
        )
    }

    /// Ensure `uri` is open, keeping its in-memory text fresh. First call sends
    /// `didOpen`; subsequent calls send a full `didChange` so analysis tracks
    /// disk edits made since the file was last read.
    pub async fn open_or_update(&self, uri: &str, text: String) -> Result<()> {
        let mut open = self.open_files.lock().await;
        // A fresh didOpen/didChange invalidates the previous diagnostic push —
        // drop the stale set so the poll below waits for the server's answer
        // about the NEW text instead of instantly returning the old one.
        self.diagnostics.write().await.remove(uri);
        if let Some(version) = open.get_mut(uri) {
            *version += 1;
            let version = *version;
            drop(open);
            // VersionedTextDocumentIdentifier.version has no serde default —
            // strict servers (rust-analyzer) drop a didChange without it.
            return self
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await;
        }
        let item = TextDocumentItem {
            uri: uri.parse().context("invalid file uri")?,
            language_id: self.language_id.clone(),
            version: 1,
            text,
        };
        let params = DidOpenTextDocumentParams {
            text_document: item,
        };
        self.notify("textDocument/didOpen", serde_json::to_value(params)?)
            .await?;
        open.insert(uri.to_string(), 1);
        Ok(())
    }

    /// documentSymbol result (raw Value; see the tool for formatting).
    pub async fn symbols(&self, uri: &str) -> Result<Value> {
        let params = lsp_types::DocumentSymbolParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.parse()? },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        self.request("textDocument/documentSymbol", serde_json::to_value(params)?)
            .await
    }

    /// `textDocument/definition` for the position (0-based UTF-16 line/char,
    /// already converted by the tool). Raw Value; see the tool for formatting.
    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    /// `textDocument/references` (always includes the declaration).
    pub async fn references(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true },
            }),
        )
        .await
    }

    /// `textDocument/implementation`.
    pub async fn implementation(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/implementation",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    /// `textDocument/hover`.
    pub async fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )
        .await
    }

    /// `textDocument/rename` — returns a `WorkspaceEdit` (raw Value); the
    /// tool layer applies it to disk, which is the client-side half of
    /// `workspace/applyEdit`.
    pub async fn rename(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Value> {
        self.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "newName": new_name,
            }),
        )
        .await
    }

    /// `textDocument/codeAction` at a position (invoked trigger, no
    /// diagnostics carried — servers may still return actions).
    pub async fn code_actions(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character },
                },
                "context": { "diagnostics": [], "triggerKind": 1 },
            }),
        )
        .await
    }

    /// `workspace/symbol` — workspace-wide symbol search; the index lives in
    /// the server (rust-analyzer keeps one), the client holds no cache.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Value> {
        self.request("workspace/symbol", json!({ "query": query }))
            .await
    }

    /// Diagnostics pushed for `uri`. Polls up to `timeout` for a non-empty set,
    /// because servers emit a 0-item "clear" push before the real diagnostics.
    pub async fn diagnostics(&self, uri: &str, timeout: Duration) -> Vec<lsp_types::Diagnostic> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let diags = self.diagnostics.read().await;
                if let Some(d) = diags.get(uri) {
                    if !d.is_empty() {
                        return d.clone();
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.diagnostics
            .read()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the reader loop has ended (server exited / pipe closed). The
    /// manager uses this to drop crashed sessions instead of reusing them.
    pub fn is_reader_dead(&self) -> bool {
        self.reader_dead.load(Ordering::SeqCst)
    }

    /// Polite shutdown: `shutdown` request → `exit` notification, then reap the
    /// child (kill as a backstop if it lingers). Called when the manager evicts
    /// an idle session.
    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", json!({})).await;
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the client against a scripted fake LSP server over in-memory pipes.
    /// Returns (client, server records) — the server echoes the frames it saw.
    async fn client_with_fake_server(
        root: &Path,
    ) -> (Arc<LspClient>, tokio::task::JoinHandle<Vec<String>>) {
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);

        let server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            let mut seen = Vec::new();
            loop {
                let bytes = match read_frame(&mut reader).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = v.get("id").and_then(|i| i.as_i64());
                match method {
                    "initialize" => {
                        seen.push("initialize".into());
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "serverInfo": { "name": "fake", "version": "1" },
                                            "capabilities": { "documentSymbolProvider": true } },
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "initialized" => seen.push("initialized".into()),
                    "textDocument/didOpen" => {
                        let uri = v["params"]["textDocument"]["uri"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        seen.push(format!("open:{uri}"));
                        // Push diagnostics immediately after open (matches real servers).
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                                "params": { "uri": uri, "diagnostics": [
                                    { "range": { "start": { "line": 0, "character": 0 },
                                                  "end": { "line": 0, "character": 3 } },
                                      "severity": 1, "source": "fake", "message": "boom" },
                                ] },
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "textDocument/didChange" => {
                        seen.push("change".into());
                    }
                    "textDocument/documentSymbol" => {
                        seen.push("symbols".into());
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": [{
                                    "name": "main", "kind": 12,
                                    "range": { "start": { "line": 0, "character": 0 },
                                               "end": { "line": 3, "character": 0 } },
                                    "selectionRange": { "start": { "line": 0, "character": 0 },
                                                        "end": { "line": 0, "character": 4 } },
                                    "children": [],
                                }],
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "textDocument/definition"
                    | "textDocument/references"
                    | "textDocument/implementation" => {
                        seen.push(format!("loc:{method}"));
                        let uri = v["params"]["textDocument"]["uri"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": [{
                                    "uri": uri,
                                    "range": { "start": { "line": 0, "character": 0 },
                                               "end": { "line": 0, "character": 4 } },
                                }],
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "textDocument/hover" => {
                        seen.push("hover".into());
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {
                                    "contents": { "kind": "markdown", "value": "fn main()\n\nfake hover" },
                                },
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "textDocument/rename" => {
                        seen.push("rename".into());
                        let uri = v["params"]["textDocument"]["uri"].as_str().unwrap();
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "documentChanges": [{
                                    "textDocument": { "uri": uri, "version": null },
                                    "edits": [
                                        { "range": { "start": { "line": 0, "character": 3 },
                                                      "end": { "line": 0, "character": 7 } },
                                          "newText": "wooo" },
                                    ],
                                }] },
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "textDocument/codeAction" => {
                        seen.push("codeAction".into());
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": [{
                                    "title": "Add derive", "kind": "refactor",
                                    "edit": { "changes": {} },
                                }],
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "workspace/symbol" => {
                        seen.push("workspace-symbol".into());
                        write_frame(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": [{
                                    "name": "main", "kind": 12,
                                    "location": { "uri": "file:///w/main.rs",
                                                  "range": { "start": { "line": 0, "character": 0 },
                                                             "end": { "line": 0, "character": 4 } } },
                                }],
                            }),
                        )
                        .await
                        .unwrap();
                    }
                    "shutdown" => {
                        seen.push("shutdown".into());
                        write_frame(
                            &mut writer,
                            &json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                        )
                        .await
                        .unwrap();
                    }
                    "exit" => {
                        seen.push("exit".into());
                        break;
                    }
                    _ => {}
                }
            }
            seen
        });

        let client = LspClient::spawn(root, "rust", Box::new(to_srv), Box::new(from_srv), None)
            .await
            .expect("spawn+initialize");
        (client, server)
    }

    #[tokio::test]
    async fn handshake_initialize_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, server) = client_with_fake_server(tmp.path()).await;
        assert!(client.symbols("file:///tmp/x/main.rs").await.is_ok());
        client.shutdown().await;
        let seen = server.await.unwrap();
        assert!(seen.iter().any(|s| s == "initialize"));
        assert!(seen.iter().any(|s| s == "initialized"));
    }

    #[tokio::test]
    async fn did_open_collects_pushed_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, server) = client_with_fake_server(tmp.path()).await;
        let uri = crate::lsp::file_uri(&tmp.path().join("src/main.rs"))
            .as_str()
            .to_string();
        client
            .open_or_update(&uri, "fn main() {}".into())
            .await
            .unwrap();
        let diags = client.diagnostics(&uri, Duration::from_secs(2)).await;
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "boom");
        client.shutdown().await;
        let seen = server.await.unwrap();
        assert!(seen.contains(&format!("open:{uri}")));
    }

    #[tokio::test]
    async fn reopen_sends_did_change_not_did_open_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, server) = client_with_fake_server(tmp.path()).await;
        let uri = crate::lsp::file_uri(&tmp.path().join("src/main.rs"))
            .as_str()
            .to_string();
        client
            .open_or_update(&uri, "fn main() {}".into())
            .await
            .unwrap();
        client
            .open_or_update(&uri, "fn main() { changed }".into())
            .await
            .unwrap();
        client.shutdown().await;
        let seen = server.await.unwrap();
        let opens = seen.iter().filter(|s| s.starts_with("open:")).count();
        assert_eq!(opens, 1);
        assert!(seen.contains(&"change".to_string()));
    }

    #[tokio::test]
    async fn document_symbol_returns_nested_response() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, server) = client_with_fake_server(tmp.path()).await;
        let result = client.symbols("file:///tmp/x/main.rs").await.unwrap();
        let parsed = serde_json::from_value::<lsp_types::DocumentSymbolResponse>(result).unwrap();
        match parsed {
            lsp_types::DocumentSymbolResponse::Nested(syms) => {
                assert_eq!(syms.len(), 1);
                assert_eq!(syms[0].name, "main");
            }
            _ => panic!("expected nested"),
        }
        client.shutdown().await;
        let seen = server.await.unwrap();
        assert!(seen.contains(&"symbols".to_string()));
    }

    #[tokio::test]
    async fn request_times_out_when_server_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);
        // Server answers initialize, then swallows every frame without responding.
        let _server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            let bytes = read_frame(&mut reader).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["method"], "initialize");
            write_frame(
                &mut writer,
                &json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "capabilities": {} } }),
            )
            .await
            .unwrap();
            loop {
                match read_frame(&mut reader).await {
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        let client = LspClient::spawn(
            tmp.path(),
            "rust",
            Box::new(to_srv),
            Box::new(from_srv),
            None,
        )
        .await
        .expect("handshake completes");
        client.request_timeout_ms.store(300, Ordering::Relaxed);
        let start = std::time::Instant::now();
        let err = client.request("textDocument/hover", json!({})).await;
        assert!(err.is_err(), "expected timeout error");
        assert!(start.elapsed() >= Duration::from_millis(300) - Duration::from_millis(100));
        drop(client);
    }

    #[tokio::test]
    async fn definition_request_returns_location() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _server) = client_with_fake_server(tmp.path()).await;
        let result = client.definition("file:///tmp/x.rs", 0, 0).await.unwrap();
        let locations: Vec<lsp_types::Location> = serde_json::from_value(result).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri.as_str(), "file:///tmp/x.rs");
    }

    #[tokio::test]
    async fn hover_request_returns_markup() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _server) = client_with_fake_server(tmp.path()).await;
        let result = client.hover("file:///tmp/x.rs", 0, 0).await.unwrap();
        let hover: lsp_types::Hover = serde_json::from_value(result).unwrap();
        let lsp_types::HoverContents::Markup(m) = hover.contents else {
            panic!("expected markup hover");
        };
        assert!(m.value.contains("hover"));
    }

    #[tokio::test]
    async fn server_capabilities_gate_optionally_supported_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _server) = client_with_fake_server(tmp.path()).await;
        // The fake server advertises documentSymbolProvider only.
        assert!(client.supports("documentSymbolProvider").await);
        assert!(!client.supports("renameProvider").await);
        assert!(!client.supports("nonexistentCapability").await);
    }

    #[tokio::test]
    async fn object_form_capability_counts_as_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);
        let _server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            let bytes = read_frame(&mut reader).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            write_frame(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0", "id": v["id"],
                    "result": { "capabilities": { "renameProvider": { "prepareProvider": true } } },
                }),
            )
            .await
            .unwrap();
            loop {
                if read_frame(&mut reader).await.is_err() {
                    break;
                }
            }
        });
        let client = LspClient::spawn(
            tmp.path(),
            "rust",
            Box::new(to_srv),
            Box::new(from_srv),
            None,
        )
        .await
        .unwrap();
        assert!(client.supports("renameProvider").await);
    }

    #[tokio::test]
    async fn rename_code_action_workspace_symbol_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, server) = client_with_fake_server(tmp.path()).await;
        let edit = client
            .rename("file:///tmp/x.rs", 0, 0, "wooo")
            .await
            .unwrap();
        let changes = edit["documentChanges"].as_array().unwrap();
        assert_eq!(changes[0]["edits"][0]["newText"], "wooo");

        let actions = client.code_actions("file:///tmp/x.rs", 0, 0).await.unwrap();
        assert_eq!(actions[0]["title"], "Add derive");

        let symbols = client.workspace_symbols("main").await.unwrap();
        assert_eq!(symbols[0]["name"], "main");
        client.shutdown().await;
        let seen = server.await.unwrap();
        assert!(seen.contains(&"rename".to_string()));
        assert!(seen.contains(&"codeAction".to_string()));
        assert!(seen.contains(&"workspace-symbol".to_string()));
    }

    #[tokio::test]
    async fn server_close_unblocks_pending_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            let bytes = read_frame(&mut reader).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["method"], "initialize");
            write_frame(
                &mut writer,
                &json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "capabilities": {} } }),
            )
            .await
            .unwrap();
            // Close our write half: the client's reader sees EOF and resolves pendings.
            drop(writer);
            loop {
                if read_frame(&mut reader).await.is_err() {
                    break;
                }
            }
        });
        let client = LspClient::spawn(
            tmp.path(),
            "rust",
            Box::new(to_srv),
            Box::new(from_srv),
            None,
        )
        .await
        .expect("handshake completes");
        let err = client.request("textDocument/hover", json!({})).await;
        assert!(err.is_err(), "expected server-closed error, not a hang");
        drop(client);
        server.await.unwrap();
    }

    // ── spawn_process plumbing (no real server) ──────────────────────────────

    #[test]
    fn spawn_process_rejects_missing_binary() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("STARRIVER_LSP_BIN_RUST", "/nonexistent/ra-bin");
        let spec = super::super::builtin_servers()
            .into_iter()
            .find(|s| s.id == "rust")
            .unwrap();
        let res = rt.block_on(LspClient::spawn_process(tmp.path(), &spec));
        std::env::remove_var("STARRIVER_LSP_BIN_RUST");
        assert!(res.is_err());
    }
}
