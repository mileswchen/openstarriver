use super::config::McpServerConfig;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// Directories to prepend to a spawned stdio server's PATH. A GUI-launched
/// packaged app (Electron from Finder/Dock) inherits the minimal launchd PATH
/// (/usr/bin:/bin:/usr/sbin:/sbin): launchers installed under Homebrew, nvm,
/// volta, cargo, pipx… then fail to spawn with ENOENT and the server
/// dead-loops the discovery retry ladder forever (host.log 2026-10-03/04,
/// server "Alibaba DevOps", `npx` at /usr/local/bin invisible). Each dir is
/// only added when it exists, and the parent's PATH is kept at the tail.
fn augmented_path() -> String {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut dirs: Vec<std::path::PathBuf> = vec![
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".volta/bin"),
        home.join("Library/pnpm"),
        std::path::PathBuf::from("/opt/homebrew/bin"),
        std::path::PathBuf::from("/opt/homebrew/sbin"),
        std::path::PathBuf::from("/usr/local/bin"),
        std::path::PathBuf::from("/usr/local/sbin"),
    ];
    // nvm keeps one bin per node version; add them all (sorted for
    // determinism) — any of them can run `npx`.
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut nvms: Vec<std::path::PathBuf> =
            entries.flatten().map(|e| e.path().join("bin")).collect();
        nvms.sort();
        dirs.append(&mut nvms);
    }
    let existing = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<std::ffi::OsString> = dirs
        .into_iter()
        .filter(|d| d.is_dir())
        .map(std::path::PathBuf::into_os_string)
        .collect();
    parts.extend(
        std::env::split_paths(&existing).map(|p| p.into_os_string()),
    );
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(existing)
}

pub struct StdioConnection {
    pub name: String,
    child: Option<Child>,
    next_id: u64,
    /// Set once an `initialize` request has succeeded on this child; a fresh
    /// subprocess must re-handshake before `tools/call` per the MCP spec.
    initialized: bool,
    /// Persistent line reader over the child's stdout (BUG-59): a per-call
    /// BufReader dropped buffered notification lines on the floor and could
    /// return a notification as this request's response.
    stdout_reader:
        Option<BufReader<tokio::process::ChildStdout>>,
}

impl StdioConnection {
    pub fn new(name: String) -> Self {
        Self {
            name,
            child: None,
            next_id: 1,
            initialized: false,
            stdout_reader: None,
        }
    }

    pub fn is_launched(&self) -> bool {
        self.child.is_some()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Kill the subprocess and clear all protocol state so the next request
    /// lazily relaunches and re-handshakes. Used after an aborted in-flight
    /// request leaves the single request/response stream desynchronized.
    pub fn reset(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
        self.child = None;
        self.initialized = false;
        self.stdout_reader = None;
    }

    pub async fn launch(&mut self, config: &McpServerConfig) -> anyhow::Result<()> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        // Applied BEFORE the server's env block so a user-set PATH there still
        // wins. On Unix this also feeds the program lookup itself (std resolves
        // the command against the child PATH), which is the actual fix.
        cmd.env("PATH", augmented_path());
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let child = cmd.spawn()?;
        self.child = Some(child);
        Ok(())
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // BUG-59: take the child's stdout ONCE into a persistent reader —
        // a per-call BufReader lost whatever it had buffered beyond one line.
        if self.stdout_reader.is_none() {
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("process not launched"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("stdout not available"))?;
            self.stdout_reader = Some(BufReader::new(stdout));
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("process not launched"))?;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin not available"))?;

        let mut req_str = serde_json::to_string(&request)?;
        req_str.push('\n');
        stdin.write_all(req_str.as_bytes()).await?;
        stdin.flush().await?;

        // BUG-136: the MCP handshake's second step — the client MUST send
        // `notifications/initialized` after a successful `initialize`, or
        // spec-strict servers (official SDK) never answer later requests.
        // Notifications carry no id and get no response; the read loop's
        // method-skip (BUG-59) already tolerates any server chatter.
        if method == "initialize" {
            let mut note = serde_json::to_string(
                &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )?;
            note.push('\n');
            stdin.write_all(note.as_bytes()).await?;
            stdin.flush().await?;
        }

        // BUG-59: skip server notifications and match the response by id —
        // JSON-RPC servers may interleave notifications with responses.
        let reader = self
            .stdout_reader
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdout reader unavailable"))?;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                anyhow::bail!("MCP server closed stdout before responding");
            }
            let message: serde_json::Value = serde_json::from_str(line.trim())?;
            // BUG-59: skip notifications and server-initiated requests
            // (anything with a `method` field). This client has exactly ONE
            // outstanding request at a time, so the next id-bearing
            // result/error message IS its response — strict id equality
            // would hang against servers that answer with arbitrary ids.
            if message.get("method").is_some() {
                continue;
            }
            let _ = id;
            if let Some(err) = message.get("error") {
                anyhow::bail!("MCP error: {err}");
            }
            if method == "initialize" && message.get("result").is_some() {
                self.initialized = true;
            }
            return Ok(message["result"].clone());
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
    }
}

/// A Streamable HTTP connection to a remote MCP endpoint.
/// JSON-RPC is sent as HTTP POST; responses may be `application/json` or
/// `text/event-stream` (SSE `data:` lines carrying the JSON-RPC message).
pub struct HttpConnection {
    pub name: String,
    client: reqwest::Client,
    base_url: String,
    token_provider: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    session_id: Option<String>,
    next_id: u64,
    last_error: String,
    initialized: bool,
}

impl HttpConnection {
    pub fn new(
        name: String,
        token_provider: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> Self {
        Self {
            name,
            client: reqwest::Client::new(),
            base_url: String::new(),
            token_provider,
            session_id: None,
            next_id: 1,
            last_error: String::new(),
            initialized: false,
        }
    }

    pub fn set_url(&mut self, url: String) {
        self.base_url = url;
    }

    pub fn is_launched(&self) -> bool {
        !self.base_url.is_empty()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn last_error(&self) -> &str {
        &self.last_error
    }

    fn build_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> reqwest::RequestBuilder {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // MCP Streamable HTTP requires Accept to list both media types; official SDK servers otherwise answer 406.
        let mut req = self
            .client
            .post(&self.base_url)
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .json(&body);
        if let Some(token) = (self.token_provider)() {
            req = req.bearer_auth(token);
        }
        if let Some(ref sid) = self.session_id {
            req = req.header("Mcp-Session-Id", sid.clone());
        }
        req
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if self.base_url.is_empty() {
            anyhow::bail!("http connection has no url");
        }
        let req = self.build_request(method, params);
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MCP http request failed: {e}"))?;

        // Capture a session id returned by the server for subsequent requests.
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("MCP http read failed: {e}"))?;

        if !status.is_success() {
            self.last_error = format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes));
            anyhow::bail!("{}", self.last_error);
        }

        let result = if content_type.contains("text/event-stream") {
            let text = String::from_utf8_lossy(&bytes);
            let val = parse_sse_response(&text)
                .ok_or_else(|| anyhow::anyhow!("MCP http: no JSON-RPC message in SSE stream"))?;
            extract_result(val)?
        } else {
            let val: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("MCP http: bad JSON response: {e}"))?;
            extract_result(val)?
        };
        if method == "initialize" {
            self.initialized = true;
            // BUG-136: the handshake's mandatory second step — send
            // `notifications/initialized` right after the successful
            // `initialize`, or spec-strict servers (official SDK) accept the
            // session but never answer tools/list (observed hanging against
            // nexplan's HTTP endpoint). The notification carries no id; the
            // server answers 202 with an empty body, which we drain and
            // ignore.
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            });
            let mut req = self
                .client
                .post(&self.base_url)
                .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
                .json(&note);
            if let Some(token) = (self.token_provider)() {
                req = req.bearer_auth(token);
            }
            if let Some(ref sid) = self.session_id {
                req = req.header("Mcp-Session-Id", sid.clone());
            }
            match req.send().await {
                Ok(resp) => {
                    let _ = resp.bytes().await;
                }
                Err(e) => {
                    self.last_error = format!("initialized notification failed: {e}");
                }
            }
        }
        Ok(result)
    }

    pub async fn shutdown(&mut self) {
        // No persistent connection to tear down; drop session state.
        self.session_id = None;
    }
}

fn extract_result(val: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    if let Some(err) = val.get("error") {
        anyhow::bail!("MCP error: {err}");
    }
    Ok(val
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// Parse a Streamable HTTP SSE response body for the first JSON-RPC message carrying a
/// `result` or `error`. Returns None if no such message is present.
fn parse_sse_response(text: &str) -> Option<serde_json::Value> {
    for chunk in text.split("\n\n") {
        for line in chunk.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(data) {
                    if val.get("result").is_some() || val.get("error").is_some() {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

/// A transport-agnostic connection used by the tool adapter.
pub enum Connection {
    Stdio(StdioConnection),
    Http(HttpConnection),
}

impl Connection {
    pub fn name(&self) -> &str {
        match self {
            Connection::Stdio(c) => &c.name,
            Connection::Http(c) => &c.name,
        }
    }

    pub fn is_launched(&self) -> bool {
        match self {
            Connection::Stdio(c) => c.is_launched(),
            Connection::Http(c) => c.is_launched(),
        }
    }

    pub fn is_initialized(&self) -> bool {
        match self {
            Connection::Stdio(c) => c.is_initialized(),
            Connection::Http(c) => c.is_initialized(),
        }
    }

    /// Reset a desynchronized stdio subprocess (kill + clear state) so the
    /// next request relaunches and re-handshakes. No-op for HTTP (each
    /// request is an independent POST; there is no stream to desync).
    pub fn reset_stdio(&mut self) {
        if let Connection::Stdio(c) = self {
            c.reset();
        }
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        match self {
            Connection::Stdio(c) => c.send_request(method, params).await,
            Connection::Http(c) => c.send_request(method, params).await,
        }
    }

    pub async fn shutdown(&mut self) {
        match self {
            Connection::Stdio(c) => c.shutdown(),
            Connection::Http(c) => c.shutdown().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn augmented_path_keeps_parent_path_at_the_tail() {
        // GUI-launched packaged app: minimal launchd PATH, launcher dirs
        // prepended in order, existing entries preserved (nothing dropped).
        std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        let path = augmented_path();
        let parts: Vec<&str> = path.split(':').collect();
        assert_eq!(*parts.last().unwrap(), "/sbin");
        assert!(parts.contains(&"/usr/bin"));
        // /usr/local/bin and /opt/homebrew/bin are prepended when present on
        // this machine; on a machine without them they are simply absent.
        if std::path::Path::new("/usr/local/bin").is_dir() {
            let local = parts.iter().position(|p| *p == "/usr/local/bin").unwrap();
            let usr_bin = parts.iter().position(|p| *p == "/usr/bin").unwrap();
            assert!(local < usr_bin, "tool dirs must precede the inherited PATH");
        }
    }

    #[test]
    fn test_stdio_connection_name() {
        let conn = StdioConnection::new("test-server".to_string());
        assert_eq!(conn.name, "test-server");
        assert!(!conn.is_launched());
    }

    #[test]
    fn test_http_connection_not_launched_until_url_set() {
        let conn = HttpConnection::new("remote".to_string(), Arc::new(|| None));
        assert_eq!(conn.name, "remote");
        assert!(!conn.is_launched());
    }

    #[test]
    fn test_connection_name_dispatch() {
        let c = Connection::Stdio(StdioConnection::new("s".to_string()));
        assert_eq!(c.name(), "s");
        assert!(!c.is_launched());
        let h = Connection::Http(HttpConnection::new("h".to_string(), Arc::new(|| None)));
        assert_eq!(h.name(), "h");
    }

    #[test]
    fn test_extract_result_returns_result_field() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}});
        let out = extract_result(v).unwrap();
        assert_eq!(out, serde_json::json!({"tools":[]}));
    }

    #[test]
    fn test_extract_result_bails_on_error() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}});
        assert!(extract_result(v).is_err());
    }

    #[test]
    fn test_extract_result_nulls_when_no_result() {
        let v = serde_json::json!({"jsonrpc":"2.0","id":1});
        assert_eq!(extract_result(v).unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn test_parse_sse_single_data_event() {
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = parse_sse_response(sse).unwrap();
        assert_eq!(v.get("result").unwrap()["ok"], true);
    }

    #[test]
    fn test_parse_sse_skips_done_and_empty() {
        let sse =
            "data: [DONE]\n\ndata:\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":\"x\"}\n\n";
        let v = parse_sse_response(sse).unwrap();
        assert_eq!(v.get("result").unwrap(), "x");
    }

    #[test]
    fn test_parse_sse_returns_none_when_no_message() {
        assert!(parse_sse_response("data: [DONE]\n\n").is_none());
        assert!(parse_sse_response("not sse").is_none());
    }
}

// ── BUG-59 regression (round 5): stdio framing under server notifications ──
#[cfg(test)]
mod round5_tests {
    use super::*;

    fn notifying_server() -> McpServerConfig {
        // Prints a server notification BEFORE the id-matched response — the
        // JSON-RPC framing every stdio server may emit.
        McpServerConfig {
            transport: crate::config::TransportType::Stdio,
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "while read l; do printf '%s\\n%s\\n' \
                 '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}' \
                 '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}'; done"
                    .into(),
            ],
            env: Default::default(),
            url: None,
            oauth: None,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        }
    }

    #[tokio::test]
    async fn notification_before_response_is_skipped_not_consumed() {
        let mut conn = StdioConnection::new("notify".into());
        conn.launch(&notifying_server()).await.expect("launch");
        let result = conn
            .send_request("tools/list", serde_json::json!({}))
            .await
            .expect("request must succeed");
        assert!(
            !result.is_null(),
            "the id-matched response result must be returned, not the notification's missing result"
        );
        assert_eq!(result["tools"].as_array().map(Vec::len), Some(0));
    }
}
