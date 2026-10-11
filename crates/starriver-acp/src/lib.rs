//! WI-70: ACP (Agent Client Protocol) server — a stdio frontend onto the
//! host runtime so editors and scripts can drive starriver unattended.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over the process's stdio
//! (stdout is protocol-only; all logging goes to stderr). The client spawns
//! this binary as a child process and speaks ACP: initialize → session/new
//! → session/prompt … with `session/update` notifications flowing back and
//! `session/request_permission` requests for approvals.
//!
//! Everything below the vocabulary is the existing [`HostRuntime`]: the
//! translation tables live in `docs/design/acp-server-design.md`.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::oneshot;

pub const PROTOCOL_VERSION: u64 = 1;

/// Inbound half of the transport. Owned exclusively by the server's read
/// loop — it may block freely.
pub trait AcpRead: Send {
    /// Next inbound protocol line (`None` on EOF — the server stops).
    fn read_line(&mut self) -> Option<String>;
}

/// Outbound half of the transport. Shared by the dispatch loop and every
/// translator thread, so implementations must be internally synchronized
/// (or stateless like stdout).
pub trait AcpWrite: Send + Sync {
    /// Emit one protocol line (must be a single line, no trailing newline).
    fn write_line(&self, line: &str);
}

/// The production transport: stdin in, stdout out.
pub struct StdioRead;

impl AcpRead for StdioRead {
    fn read_line(&mut self) -> Option<String> {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match lock.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    if !line.trim().is_empty() {
                        return Some(line);
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

pub struct StdioWrite;

impl AcpWrite for StdioWrite {
    fn write_line(&self, line: &str) {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(line.as_bytes());
        let _ = lock.write_all(b"\n");
        let _ = lock.flush();
    }
}

// JSON-RPC error codes used below.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const APP_ERROR: i64 = -32000;

/// One running ACP server instance over a transport.
pub struct AcpServer {
    host: Arc<starriver_host::HostRuntime>,
    reader: Mutex<Option<Box<dyn AcpRead>>>,
    write: Arc<dyn AcpWrite>,
    initialized: Mutex<bool>,
    /// Sessions minted (or loaded) through this connection.
    sessions: Mutex<HashMap<String, ()>>,
    /// Pending server→client requests (permission asks), keyed by our
    /// request id; the main loop completes the matching oneshot when the
    /// client's response line arrives.
    outbound: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_outbound_id: AtomicU64,
    /// The client's declared capabilities from `initialize` (fs access,
    /// output styles…). Recorded for future consumers; the server currently
    /// requests nothing from the client beyond permission answers.
    client_capabilities: Mutex<Option<Value>>,
}

impl AcpServer {
    pub fn new(
        host: Arc<starriver_host::HostRuntime>,
        read: Box<dyn AcpRead>,
        write: Arc<dyn AcpWrite>,
    ) -> Self {
        Self {
            host,
            reader: Mutex::new(Some(read)),
            write,
            initialized: Mutex::new(false),
            sessions: Mutex::new(HashMap::new()),
            outbound: Mutex::new(HashMap::new()),
            next_outbound_id: AtomicU64::new(1),
            client_capabilities: Mutex::new(None),
        }
    }

    /// Serve until the transport reaches EOF. Consumes `self` on a worker
    /// thread when callers need the handle back (tests).
    pub fn spawn(self: Arc<Self>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || self.run())
    }

    pub fn run(self: Arc<Self>) {
        // The reader half moves out here — the read loop owns it exclusively
        // so a blocking read never contends with writers.
        let mut reader = self
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("AcpServer::run called twice");
        loop {
            let Some(line) = reader.read_line() else { break };
            let message: Value = match serde_json::from_str(line.trim()) {
                Ok(value) => value,
                Err(error) => {
                    self.emit(
                        json!({"jsonrpc": "2.0", "id": Value::Null,
                               "error": {"code": -32700, "message": format!("parse error: {error}")}}),
                    );
                    continue;
                }
            };
            let this = Arc::clone(&self);
            this.dispatch(message);
        }
        // EOF: drop pending permission askers so translators fail closed.
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn emit(&self, message: Value) {
        self.write.write_line(&message.to_string());
    }

    fn respond(&self, id: &Value, result: Value) {
        self.emit(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn respond_error(&self, id: &Value, code: i64, message: String) {
        self.emit(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": code, "message": message}
        }));
    }

    fn dispatch(self: &Arc<Self>, message: Value) {
        if let Some(id) = message.get("id") {
            if message.get("result").is_some() || message.get("error").is_some() {
                // A response to one of OUR requests (permission asks).
                self.complete_outbound(id.clone(), message);
                return;
            }
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return; // Not JSON-RPC traffic we act on.
        };
        let params = message.get("params").cloned().unwrap_or(json!({}));
        let id = message.get("id").cloned();
        match (method, id) {
            ("initialize", Some(id)) => {
                *self.initialized.lock().unwrap_or_else(|e| e.into_inner()) = true;
                *self.client_capabilities.lock().unwrap_or_else(|e| e.into_inner()) =
                    params.get("clientCapabilities").cloned();
                self.respond(&id, json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {"outputStyle": false},
                        // Honest declaration: the host toolchain reads the
                        // LOCAL workspace fs — the server never asks the
                        // client to read or write files.
                        "fs": {"readTextFile": false, "writeTextFile": false},
                    },
                    "authMethods": [],
                }));
            }
            ("initialize", None) => {
                *self.initialized.lock().unwrap_or_else(|e| e.into_inner()) = true;
            }
            ("initialized", _) => {}
            ("session/new", Some(id)) => match self.new_session(&params) {
                Ok(session_id) => self.respond(&id, json!({"sessionId": session_id})),
                Err((code, message)) => self.respond_error(&id, code, message),
            },
            ("session/load", Some(id)) => match self.load_session(&params) {
                Ok(session_id) => self.respond(&id, json!({"sessionId": session_id})),
                Err((code, message)) => self.respond_error(&id, code, message),
            },
            ("session/prompt", Some(id)) => {
                match self.prompt(&params, id.clone()) {
                    Ok(()) => {} // Response lands from the translator thread.
                    Err((code, message)) => self.respond_error(&id, code, message),
                }
            }
            ("session/cancel", _) => {
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                    self.host.cancel(session_id.to_string());
                }
            }
            (method, Some(id)) => {
                self.respond_error(&id, METHOD_NOT_FOUND, format!("unknown method: {method}"))
            }
            (method, None) => {
                // Unknown notifications are ignored per JSON-RPC.
                let _ = method;
            }
        }
    }

    // ── session lifecycle ─────────────────────────────────────────────────

    fn require_initialized(&self) -> Result<(), (i64, String)> {
        if *self.initialized.lock().unwrap_or_else(|e| e.into_inner()) {
            Ok(())
        } else {
            Err((APP_ERROR, "server not initialized".into()))
        }
    }

    /// Bind `cwd` as the coding agent's workspace (the ACP model is one
    /// agent process per project, so an agent-level binding matches the
    /// process lifetime exactly).
    fn bind_cwd(&self, cwd: &str) -> Result<(), (i64, String)> {
        let workspace = self
            .host
            .create_workspace(cwd.to_string())
            .map_err(|e| (INVALID_PARAMS, format!("invalid cwd `{cwd}`: {e}")))?;
        let agent = starriver_host::CODING_AGENT_NAME;
        self.host
            .update_settings(json!({
                "agent": {"active_workspaces": {agent: workspace.id}}
            }))
            .map_err(|e| (APP_ERROR, format!("workspace binding failed: {e}")))?;
        Ok(())
    }

    fn attach_mcp_servers(&self, servers: &[Value]) -> Result<(), (i64, String)> {
        for server in servers {
            let name = server
                .get("name")
                .and_then(Value::as_str)
                .ok_or((INVALID_PARAMS, "mcpServers entry missing name".into()))?;
            let command = server
                .get("command")
                .and_then(Value::as_str)
                .ok_or((INVALID_PARAMS, format!("mcp server `{name}` missing command")))?;
            let args = serde_json::to_string(&server.get("args").cloned().unwrap_or(json!([])))
                .map_err(|e| (INVALID_PARAMS, format!("mcp args: {e}")))?;
            let env = serde_json::to_string(&server.get("env").cloned().unwrap_or(json!({})))
                .map_err(|e| (INVALID_PARAMS, format!("mcp env: {e}")))?;
            self.host
                .add_mcp_server(
                    name.to_string(),
                    "stdio".into(),
                    command.to_string(),
                    args,
                    env,
                    None,
                    None,
                )
                .map_err(|e| (APP_ERROR, format!("mcp server `{name}`: {e}")))?;
        }
        Ok(())
    }

    fn new_session(&self, params: &Value) -> Result<String, (i64, String)> {
        self.require_initialized()?;
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            if !cwd.trim().is_empty() {
                self.bind_cwd(cwd)?;
            }
        }
        if let Some(servers) = params.get("mcpServers").and_then(Value::as_array) {
            self.attach_mcp_servers(servers)?;
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), ());
        Ok(session_id)
    }

    fn load_session(&self, params: &Value) -> Result<String, (i64, String)> {
        self.require_initialized()?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "sessionId required".into()))?
            .to_string();
        let known = self
            .host
            .list_sessions(None)
            .iter()
            .any(|s| s.id == session_id);
        if !known {
            return Err((APP_ERROR, format!("unknown session: {session_id}")));
        }
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            if !cwd.trim().is_empty() {
                self.bind_cwd(cwd)?;
            }
        }
        if let Some(servers) = params.get("mcpServers").and_then(Value::as_array) {
            self.attach_mcp_servers(servers)?;
        }
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), ());
        Ok(session_id)
    }

    // ── prompt ────────────────────────────────────────────────────────────

    fn prompt(self: &Arc<Self>, params: &Value, request_id: Value) -> Result<(), (i64, String)> {
        self.require_initialized()?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "sessionId required".into()))?
            .to_string();
        {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if !sessions.contains_key(&session_id) {
                // A session loaded in a previous process is still fair game.
                let known = self
                    .host
                    .list_sessions(None)
                    .iter()
                    .any(|s| s.id == session_id);
                if !known {
                    return Err((APP_ERROR, format!("unknown session: {session_id}")));
                }
            }
        }
        let prompt = params
            .get("prompt")
            .ok_or((INVALID_PARAMS, "prompt required".into()))?;
        if prompt.get("outputStyle").is_some() {
            // We advertise promptCapabilities.outputStyle = false — passing
            // one anyway is a client contract violation, reject explicitly
            // instead of silently ignoring the style.
            return Err((
                INVALID_PARAMS,
                "outputStyle is not supported (promptCapabilities.outputStyle is false)".into(),
            ));
        }
        let task = build_task(prompt).map_err(|e| (INVALID_PARAMS, e))?;
        if task.trim().is_empty() {
            return Err((INVALID_PARAMS, "prompt has no content".into()));
        }
        // Optional model selection: `provider/variant` composite, or a bare
        // variant id resolved against the configured providers.
        if let Some(model_id) = params
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
        {
            let (provider, variant) = resolve_model(self.host.as_ref(), model_id)?;
            self.host
                .set_agent_active_model(
                    starriver_host::CODING_AGENT_NAME.into(),
                    provider,
                    variant,
                )
                .map_err(|e| (APP_ERROR, format!("model switch failed: {e}")))?;
        }
        let rx = self
            .host
            .execute(
                task,
                Some(session_id.clone()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .map_err(|e| (APP_ERROR, format!("execute failed: {e}")))?;
        // The translator thread owns the prompt's JSON-RPC response (it
        // lands on the first terminal event) and keeps forwarding updates
        // for any goal continuation rounds.
        spawn_translator(Arc::clone(self), session_id, request_id, rx);
        Ok(())
    }

    fn complete_outbound(&self, id: Value, response: Value) {
        let key = id.as_u64().unwrap_or(0);
        let sender = self
            .outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if let Some(sender) = sender {
            // The translator reasons over the RESULT object (e.g.
            // `/outcome/id`), not the JSON-RPC envelope — a client-side
            // error is a rejection (fail closed).
            let payload = response
                .get("result")
                .cloned()
                .unwrap_or(json!({"outcome": {"id": "reject"}}));
            let _ = sender.send(payload);
        }
    }
}

/// Assemble the host task text from an ACP prompt: text blocks concatenate,
/// image blocks are admitted to the content-addressed store and referenced
/// inline (the host's execute extracts exactly these refs as image input).
fn build_task(prompt: &Value) -> Result<String, String> {
    let blocks = prompt
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompt.content must be an array".to_string())?;
    let mut parts: Vec<String> = Vec::new();
    let mut image_index = 0usize;
    let store = starriver_host::attachments::open_attachment_store();
    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            "image" => {
                use base64::Engine as _;
                use starriver_kernel::attachments::AttachmentStoreApi;
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "image block missing base64 data".to_string())?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| format!("image base64 invalid: {e}"))?;
                // The normalizer validates declared == actual bytes, so the
                // declared type comes from sniffing, not from the client.
                let declared = sniff_media_type(&bytes)
                    .unwrap_or_else(|| {
                        block
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string()
                    });
                let candidate = starriver_kernel::attachments::AdmissionCandidate {
                    name: Some(format!("acp-image-{}", image_index + 1)),
                    media_type: declared,
                    data_base64: data.to_string(),
                };
                let admitted =
                    AttachmentStoreApi::admit_images(store.as_ref(), &[candidate])
                        .map_err(|e| format!("image admission failed: {e}"))?;
                for admitted in admitted {
                    image_index += 1;
                    parts.push(format!("![image-{image_index}]({})", admitted.id));
                }
            }
            "resource_link" => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                parts.push(if name.is_empty() {
                    uri.to_string()
                } else {
                    format!("{name} ({uri})")
                });
            }
            other => return Err(format!("unsupported content block type: {other}")),
        }
    }
    Ok(parts.join("\n"))
}

fn sniff_media_type(bytes: &[u8]) -> Option<String> {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => Some("image/png".into()),
        Ok(image::ImageFormat::Jpeg) => Some("image/jpeg".into()),
        Ok(image::ImageFormat::WebP) => Some("image/webp".into()),
        Ok(image::ImageFormat::Gif) => Some("image/gif".into()),
        Ok(image::ImageFormat::Bmp) => Some("image/bmp".into()),
        _ => None,
    }
}

/// `provider/variant` composite, or a bare variant resolved against the
/// configured providers (first owner wins, mirroring the host's own
/// variant fallback).
fn resolve_model(
    host: &starriver_host::HostRuntime,
    model_id: &str,
) -> Result<(String, String), (i64, String)> {
    if let Some((provider, variant)) = model_id.split_once('/') {
        return Ok((provider.to_string(), variant.to_string()));
    }
    let settings = host.get_settings_json();
    for provider in settings
        .get("llm")
        .and_then(|llm| llm.get("providers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = provider.get("id").and_then(Value::as_str).unwrap_or("");
        let default_model = provider
            .get("default_model")
            .and_then(Value::as_str)
            .unwrap_or("");
        let variants = provider
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .any(|m| m.get("id").and_then(Value::as_str) == Some(model_id))
            })
            .unwrap_or(false);
        if variants || (!default_model.is_empty() && default_model == model_id) {
            return Ok((id.to_string(), model_id.to_string()));
        }
    }
    Err((
        INVALID_PARAMS,
        format!("unknown model `{model_id}` (expected `provider/variant`)"),
    ))
}

/// The tool "kind" bucket ACP assigns to a tool call row.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "grep" | "glob" | "ls" => "read",
        "bash" | "edit" | "write" | "patch" => "execute",
        "web_search" | "web_fetch" | "search" => "search",
        _ => "other",
    }
}

fn short(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}

/// Run the event translator for one prompt: forwards `session/update`
/// notifications, services the permission bridge, and answers the prompt
/// request on the first terminal event (later goal rounds keep streaming
/// as plain updates — ACP allows notifications at any time).
pub fn spawn_translator(
    server: Arc<AcpServer>,
    session_id: String,
    request_id: Value,
    mut rx: tokio::sync::broadcast::Receiver<Value>,
) {
    std::thread::spawn(move || {
        let host = Arc::clone(&server.host);
        let handle = host.runtime_handle().clone();
        handle.block_on(async move {
            let mut resolved = false;
            // An event that won the race against a permission answer (the
            // turn moved on — cancelled, or the approval resolved another
            // way); processed first on the next loop pass.
            let mut queued: Option<Value> = None;
            loop {
                let event = match queued.take() {
                    Some(event) => event,
                    None => match rx.recv().await {
                        Ok(event) => event,
                        Err(_) => {
                            if !resolved {
                                resolved = true;
                                server.respond(&request_id, json!({"stopReason": "end_turn"}));
                            }
                            break;
                        }
                    },
                };
                let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "assistantTextDelta" => {
                        let text = event.get("text").and_then(Value::as_str).unwrap_or("");
                        server.emit(session_update(
                            &session_id,
                            json!({"type": "agent_message_chunk",
                                   "content": [{"type": "text", "text": text}]}),
                        ));
                    }
                    "assistantText" => {
                        let text = event.get("text").and_then(Value::as_str).unwrap_or("");
                        server.emit(session_update(
                            &session_id,
                            json!({"type": "agent_message",
                                   "content": [{"type": "text", "text": text}]}),
                        ));
                    }
                    "toolCall" => {
                        let call_id = event.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = event.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = event.get("input").and_then(Value::as_str).unwrap_or("");
                        let title = if input.is_empty() {
                            name.to_string()
                        } else {
                            format!("{name}: {}", short(input, 40))
                        };
                        server.emit(session_update(
                            &session_id,
                            json!({"type": "tool_call", "toolCallId": call_id,
                                   "title": title, "kind": tool_kind(name),
                                   "status": "in_progress", "content": []}),
                        ));
                    }
                    "toolResult" => {
                        let call_id = event.get("id").and_then(Value::as_str).unwrap_or("");
                        let ok = event.get("ok").and_then(Value::as_bool).unwrap_or(true);
                        let output = event.get("output").and_then(Value::as_str).unwrap_or("");
                        let head = format!("{} {}", if ok { "✓" } else { "✗" }, short(output, 400));
                        server.emit(session_update(
                            &session_id,
                            json!({"type": "tool_call_update", "toolCallId": call_id,
                                   "status": "completed",
                                   "content": [{"type": "text", "text": head}]}),
                        ));
                    }
                    "planReviewRequested" => {
                        // WI-111: surface the host's plan-review flow as an
                        // ACP plan block — the client renders its plan panel.
                        server.emit(session_update(
                            &session_id,
                            plan_review_requested_update(&event),
                        ));
                    }
                    "planReviewDecided" => {
                        server.emit(session_update(
                            &session_id,
                            plan_review_decided_update(&event),
                        ));
                    }
                    "approvalRequested" => {
                        // Ask the client; the main loop routes the response
                        // line back through the outbound map.
                        let (tx, outcome_rx) = oneshot::channel();
                        let id = server.next_outbound_id.fetch_add(1, Ordering::SeqCst);
                        server
                            .outbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id, tx);
                        let call_id = event.get("callId").and_then(Value::as_str);
                        let mut params = json!({
                            "sessionId": session_id,
                            "options": [
                                {"id": "allow_once", "name": "Allow", "kind": "once"},
                                {"id": "allow_always", "name": "Always allow", "kind": "always"},
                                {"id": "reject", "name": "Reject", "kind": "once"},
                            ]
                        });
                        if let Some(call_id) = call_id {
                            params["toolCallId"] = json!(call_id);
                        }
                        server.emit(json!({
                            "jsonrpc": "2.0", "id": id,
                            "method": "session/request_permission",
                            "params": params
                        }));
                        // Race the answer against the event stream: the
                        // host may resolve the approval without the client
                        // (a cancel aborts the ask), and blocking here
                        // would starve every later event.
                        let outcome = tokio::select! {
                            biased;
                            outcome = outcome_rx => {
                                // A dropped sender (EOF / superseded) rejects.
                                let outcome = outcome
                                    .unwrap_or_else(|_| json!({"outcome": {"id": "reject"}}));
                                let choice = outcome
                                    .pointer("/outcome/id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("reject");
                                let allowed = choice != "reject";
                                let remember = choice == "allow_always";
                                let sid = session_id.clone();
                                let host = Arc::clone(&server.host);
                                let _ = tokio::task::spawn_blocking(move || {
                                    host.approve(sid, allowed, Some(remember));
                                })
                                .await;
                            }
                            event = rx.recv() => match event {
                                Ok(event) => {
                                    // The turn moved on without the client's
                                    // answer — process the event normally and
                                    // abandon this ask (a late answer hits
                                    // APPROVAL_NOT_PENDING and is a no-op).
                                    queued = Some(event);
                                }
                                Err(_) => {
                                    if !resolved {
                                        resolved = true;
                                        server.respond(&request_id, json!({"stopReason": "end_turn"}));
                                    }
                                    break;
                                }
                            },
                        };
                        let _ = outcome;
                    }
                    "turnEnd" => {
                        if !resolved {
                            resolved = true;
                            server.respond(&request_id, json!({"stopReason": "end_turn"}));
                        }
                        // Keep translating: goal continuation rounds may
                        // follow and their updates remain meaningful.
                    }
                    "cancelled" => {
                        if !resolved {
                            resolved = true;
                            server.respond(&request_id, json!({"stopReason": "cancelled"}));
                        }
                    }
                    "turnError" | "TaskFailed" => {
                        let detail = event
                            .get("error")
                            .and_then(Value::as_str)
                            .or_else(|| event.get("text").and_then(Value::as_str))
                            .unwrap_or("turn failed");
                        server.emit(session_update(
                            &session_id,
                            json!({"type": "agent_message_chunk",
                                   "content": [{"type": "text", "text": format!("⚠ {detail}")}]}),
                        ));
                        if !resolved {
                            resolved = true;
                            server.respond(&request_id, json!({"stopReason": "end_turn"}));
                        }
                    }
                    _ => {}
                }
            }
        });
    });
}


/// WI-111: `planReviewRequested` → an ACP `plan` update (pending, carrying
/// the plan markdown as its content).
fn plan_review_requested_update(event: &Value) -> Value {
    let request_id = event.get("requestId").and_then(Value::as_str).unwrap_or("");
    let plan_text = event.get("plan").and_then(Value::as_str).unwrap_or("");
    json!({"type": "plan", "plan": {
        "id": request_id,
        "title": "Plan review",
        "status": "pending",
        "content": [{"type": "text", "text": plan_text}],
    }})
}

/// WI-111: `planReviewDecided` → the same plan flips to completed on
/// approval (a rejection leaves it pending for the next revision round).
fn plan_review_decided_update(event: &Value) -> Value {
    let request_id = event.get("requestId").and_then(Value::as_str).unwrap_or("");
    let approved = event.get("approved").and_then(Value::as_bool).unwrap_or(false);
    json!({"type": "plan", "plan": {
        "id": request_id,
        "title": "Plan review",
        "status": if approved { "completed" } else { "pending" },
        "content": [],
    }})
}

fn session_update(session_id: &str, update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update",
           "params": {"sessionId": session_id, "update": update}})
}


#[cfg(test)]
mod tests {
    use super::*;

    /// WI-111: plan-review events translate to ACP plan blocks — requested
    /// carries the markdown as pending content, approval flips to
    /// completed, rejection stays pending.
    #[test]
    fn plan_events_translate_to_acp_plan_blocks() {
        let requested = plan_review_requested_update(&json!({
            "type": "planReviewRequested", "requestId": "pr-1",
            "plan": "# Plan\n1. step",
        }));
        assert_eq!(requested["type"], "plan");
        assert_eq!(requested["plan"]["id"], "pr-1");
        assert_eq!(requested["plan"]["status"], "pending");
        assert_eq!(requested["plan"]["content"][0]["text"], "# Plan\n1. step");

        let approved = plan_review_decided_update(&json!({
            "type": "planReviewDecided", "requestId": "pr-1", "approved": true,
        }));
        assert_eq!(approved["plan"]["status"], "completed");

        let rejected = plan_review_decided_update(&json!({
            "type": "planReviewDecided", "requestId": "pr-1", "approved": false,
        }));
        assert_eq!(rejected["plan"]["status"], "pending");
    }
}
