//! Minimal E2B cloud-sandbox client (Rust port of the upstream JS client,
//! native).
//!
//! E2B splits its surface into two planes:
//! - **Control plane** (REST, `https://api.e2b.app`): sandbox lifecycle.
//!   `POST /sandboxes` creates a sandbox from a template; `DELETE
//!   /sandboxes/{sandboxID}` disposes it. Authenticated with `X-API-Key`.
//! - **Data plane** (envd inside the sandbox, Connect protocol over HTTPS):
//!   process execution. `process.Process/Start` starts `/bin/bash -l -c` and
//!   streams `ProcessEvent` JSON messages (start/data/end/keepalive),
//!   authenticated with the sandbox-scoped `X-Access-Token`.
//!
//! There is no maintained official Rust SDK, so this module implements the
//! wire protocol directly with reqwest: the REST calls are plain JSON, and the
//! Connect server-streaming RPC uses the standard envelope framing
//! (`1 flag byte + 4-byte big-endian length + JSON payload`), with an
//! end-stream envelope carrying errors.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

/// envd listens on this port inside every sandbox.
pub const ENVD_PORT: u16 = 49983;
/// Default sandbox lifetime in seconds (matches the official SDK default).
pub const DEFAULT_SANDBOX_TIMEOUT_SECS: u64 = 300;
/// Timeout for individual control-plane requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout for the sandbox health probe.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for one Connect envelope payload (64 MiB).
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Connect envelope flags (see https://connectrpc.com/docs/protocol).
const FLAG_COMPRESSED: u8 = 0b1000_0000;
const FLAG_END_STREAM: u8 = 0b0000_0010;

/// Domains that serve the stable `sandbox.<domain>` envd host.
const STABLE_SANDBOX_DOMAINS: [&str; 4] = ["e2b.app", "e2b.dev", "e2b.pro", "e2b-staging.dev"];

/// Resolved E2B connection configuration.
#[derive(Debug, Clone)]
pub struct E2BConfig {
    pub api_key: String,
    pub api_url: String,
    pub sandbox_url_override: Option<String>,
    pub domain: String,
    pub template: String,
    pub sandbox_timeout_secs: u64,
}

impl E2BConfig {
    /// Build the config from the environment, mirroring the official SDK's
    /// env vars: `E2B_API_KEY`, `E2B_API_URL`, `E2B_SANDBOX_URL`,
    /// `E2B_DOMAIN`, `E2B_TEMPLATE`, `E2B_SANDBOX_TIMEOUT_MS`.
    pub fn from_env() -> Result<Self, E2BError> {
        let api_key = std::env::var("E2B_API_KEY").map_err(|_| {
            E2BError::Unconfigured(
                "E2B_API_KEY is not set — configure a remote sandbox key to enable e2b_run".into(),
            )
        })?;
        let domain = std::env::var("E2B_DOMAIN").unwrap_or_else(|_| "e2b.app".to_string());
        let api_url =
            std::env::var("E2B_API_URL").unwrap_or_else(|_| format!("https://api.{domain}"));
        let sandbox_url_override = std::env::var("E2B_SANDBOX_URL").ok();
        let template = std::env::var("E2B_TEMPLATE").unwrap_or_else(|_| "base".to_string());
        let sandbox_timeout_secs = std::env::var("E2B_SANDBOX_TIMEOUT_MS")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            // BUG-65: an explicit sub-second timeout clamps DOWN to 1s —
            // floor-to-zero + fallback silently widened it to the 300s
            // default, the opposite of the operator's intent.
            .map(|ms| (ms / 1000).max(1))
            .unwrap_or(DEFAULT_SANDBOX_TIMEOUT_SECS);
        Ok(Self {
            api_key,
            api_url,
            sandbox_url_override,
            domain,
            template,
            sandbox_timeout_secs,
        })
    }

    /// envd base URL for a sandbox: the stable `sandbox.<domain>` host on
    /// known domains, otherwise the per-sandbox `{port}-{id}.{domain}` host.
    pub fn envd_url(&self, sandbox: &E2BSandbox) -> String {
        if let Some(override_url) = &self.sandbox_url_override {
            return override_url.trim_end_matches('/').to_string();
        }
        let domain = sandbox.domain.as_deref().unwrap_or(&self.domain);
        if STABLE_SANDBOX_DOMAINS.contains(&domain) {
            format!("https://sandbox.{domain}")
        } else {
            format!("https://{ENVD_PORT}-{}.{domain}", sandbox.sandbox_id)
        }
    }
}

/// A live E2B sandbox handle returned by the control plane.
#[derive(Debug, Clone)]
pub struct E2BSandbox {
    pub sandbox_id: String,
    pub domain: Option<String>,
    pub envd_version: String,
    pub envd_access_token: String,
    pub traffic_access_token: Option<String>,
}

/// Result of one remote command execution.
#[derive(Debug, Clone)]
pub struct E2BCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Errors surfaced by the E2B adapter, aligned with the SDK error taxonomy.
#[derive(Debug, thiserror::Error)]
pub enum E2BError {
    #[error("E2B is not configured: {0}")]
    Unconfigured(String),
    #[error("E2B authentication failed: {0}")]
    Auth(String),
    #[error("E2B resource not found: {0}")]
    NotFound(String),
    #[error("E2B rate limit exceeded: {0}")]
    RateLimit(String),
    #[error("E2B sandbox unavailable or timed out: {0}")]
    Timeout(String),
    #[error("E2B sandbox ran out of space: {0}")]
    NotEnoughSpace(String),
    #[error("E2B request rejected: {0}")]
    InvalidInput(String),
    #[error("E2B sandbox error: {0}")]
    Sandbox(String),
    #[error("E2B command failed (exit {exit_code}): {output}")]
    CommandFailed { exit_code: i32, output: String },
}

impl E2BError {
    /// Stable tool-failure code for the shell surface.
    pub fn code(&self) -> &'static str {
        match self {
            E2BError::Unconfigured(_) => "UNCONFIGURED",
            E2BError::Auth(_) => "AUTH_FAILED",
            E2BError::NotFound(_) => "NOT_FOUND",
            E2BError::RateLimit(_) => "RATE_LIMITED",
            E2BError::Timeout(_) => "TIMEOUT",
            E2BError::NotEnoughSpace(_) => "NO_SPACE",
            E2BError::InvalidInput(_) => "INVALID_INPUT",
            E2BError::Sandbox(_) => "SANDBOX_ERROR",
            E2BError::CommandFailed { .. } => "EXECUTION_FAILED",
        }
    }
}

/// E2B client: REST control plane plus the envd Connect data plane.
#[derive(Debug, Clone)]
pub struct E2BClient {
    config: E2BConfig,
    http: Client,
}

impl E2BClient {
    pub fn new(config: E2BConfig) -> Self {
        let http = Client::builder()
            .user_agent("starriver-e2b/0.1")
            .build()
            .expect("reqwest client builds");
        Self { config, http }
    }

    pub fn config(&self) -> &E2BConfig {
        &self.config
    }

    /// Create a sandbox from the configured template.
    pub async fn create_sandbox(&self) -> Result<E2BSandbox, E2BError> {
        let url = format!("{}/sandboxes", self.config.api_url.trim_end_matches('/'));
        let body = json!({
            "templateID": self.config.template,
            "timeout": self.config.sandbox_timeout_secs,
            "secure": true,
            "allow_internet_access": true,
        });
        let response = self
            .http
            .post(&url)
            .header("X-API-Key", &self.config.api_key)
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| E2BError::Sandbox(format!("create request failed: {e}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        if !status.is_success() {
            return Err(map_rest_error(status, &text));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| E2BError::Sandbox(format!("invalid create response ({status}): {e}")))?;
        let sandbox_id = parsed
            .get("sandboxID")
            .and_then(Value::as_str)
            .ok_or_else(|| E2BError::Sandbox(format!("create response missing sandboxID: {text}")))?
            .to_string();
        let envd_version = parsed
            .get("envdVersion")
            .and_then(Value::as_str)
            .unwrap_or("0.0.0")
            .to_string();
        let envd_access_token = parsed
            .get("envdAccessToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if envd_access_token.is_empty() {
            return Err(E2BError::Sandbox(format!(
                "create response missing envdAccessToken: {text}"
            )));
        }
        Ok(E2BSandbox {
            sandbox_id,
            domain: parsed
                .get("domain")
                .and_then(Value::as_str)
                .map(String::from),
            envd_version,
            envd_access_token,
            traffic_access_token: parsed
                .get("trafficAccessToken")
                .and_then(Value::as_str)
                .map(String::from),
        })
    }

    /// Dispose a sandbox. Returns `false` when it is already gone (404).
    pub async fn kill_sandbox(&self, sandbox_id: &str) -> Result<bool, E2BError> {
        let url = format!(
            "{}/sandboxes/{sandbox_id}",
            self.config.api_url.trim_end_matches('/')
        );
        let response = self
            .http
            .delete(&url)
            .header("X-API-Key", &self.config.api_key)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| E2BError::Sandbox(format!("kill request failed: {e}")))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        if !status.is_success() {
            return Err(map_rest_error(status, &text));
        }
        Ok(true)
    }

    /// Run one command in the sandbox and wait for its completion.
    pub async fn run_command(
        &self,
        sandbox: &E2BSandbox,
        command: &str,
        cwd: Option<&str>,
        envs: &BTreeMap<String, String>,
    ) -> Result<E2BCommandResult, E2BError> {
        let url = format!("{}/process.Process/Start", self.config.envd_url(sandbox));
        let mut process = json!({
            "cmd": "/bin/bash",
            "args": ["-l", "-c", command],
            "envs": envs,
        });
        if let Some(cwd) = cwd {
            process["cwd"] = json!(cwd);
        }
        let message = json!({
            "process": process,
            "stdin": false,
        });
        let payload = serde_json::to_vec(&message)
            .map_err(|e| E2BError::Sandbox(format!("encode start request: {e}")))?;
        let mut body = Vec::with_capacity(payload.len() + 5);
        body.push(0);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&payload);

        let response = match self
            .http
            .post(&url)
            .header("Content-Type", "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .header("Keepalive-Ping-Interval", "50")
            .header("X-Access-Token", &sandbox.envd_access_token)
            .header("E2b-Sandbox-Id", &sandbox.sandbox_id)
            .header("E2b-Sandbox-Port", ENVD_PORT.to_string())
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return Err(self.map_transport_error(sandbox, error).await),
        };
        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(map_connect_http_error(status, &text));
        }

        self.collect_command_stream(sandbox, response).await
    }

    /// Probe the envd health endpoint to tell a dead sandbox apart from a
    /// transient network failure. `Ok(Some(true))` = up, `Ok(Some(false))` =
    /// confirmed down, `Ok(None)` = state unknown.
    pub async fn check_health(&self, sandbox: &E2BSandbox) -> Result<Option<bool>, E2BError> {
        let url = format!("{}/health", self.config.envd_url(sandbox));
        let response = match self
            .http
            .get(&url)
            .header("X-Access-Token", &sandbox.envd_access_token)
            .header("E2b-Sandbox-Id", &sandbox.sandbox_id)
            .header("E2b-Sandbox-Port", ENVD_PORT.to_string())
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };
        if response.status() == StatusCode::BAD_GATEWAY {
            return Ok(Some(false));
        }
        if response.status().is_success() {
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// Turn a transport-level failure into a typed error, using the health
    /// probe to recognize a sandbox that died mid-request.
    async fn map_transport_error(&self, sandbox: &E2BSandbox, error: reqwest::Error) -> E2BError {
        if matches!(self.check_health(sandbox).await, Ok(Some(false))) {
            return E2BError::Timeout(format!(
                "the sandbox was killed or reached its end of life while the request was in flight ({error})"
            ));
        }
        E2BError::Sandbox(format!("envd request failed: {error}"))
    }

    /// Read the Connect envelope stream until the end-stream marker, keeping
    /// stdout/stderr and the final exit code.
    async fn collect_command_stream(
        &self,
        sandbox: &E2BSandbox,
        response: reqwest::Response,
    ) -> Result<E2BCommandResult, E2BError> {
        let mut reader = EnvelopeReader::new();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut exit_code = -1;
        let mut saw_exit = false;
        let mut stream = response.bytes_stream();

        loop {
            if let Some(message) = reader.next_message() {
                match message {
                    Ok(EnvelopeMessage::Data(payload)) => {
                        let parsed: Value = serde_json::from_slice(&payload)
                            .map_err(|e| E2BError::Sandbox(format!("bad event JSON: {e}")))?;
                        let event = parsed.get("event").ok_or_else(|| {
                            E2BError::Sandbox("start event missing `event` field".into())
                        })?;
                        if let Some(start) = event.get("start") {
                            let _pid = start.get("pid").and_then(Value::as_u64);
                        } else if let Some(data) = event.get("data") {
                            if let Some(bytes) = decode_base64_field(data, "stdout")? {
                                stdout.extend_from_slice(&bytes);
                            }
                            if let Some(bytes) = decode_base64_field(data, "stderr")? {
                                stderr.extend_from_slice(&bytes);
                            }
                        } else if let Some(end) = event.get("end") {
                            saw_exit = true;
                            exit_code =
                                end.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                            let exited = end.get("exited").and_then(Value::as_bool).unwrap_or(true);
                            if let Some(message) = end.get("error").and_then(Value::as_str) {
                                if !message.is_empty() {
                                    stderr.extend_from_slice(message.as_bytes());
                                    stderr.push(b'\n');
                                }
                            }
                            if exited {
                                break;
                            }
                        }
                    }
                    Ok(EnvelopeMessage::EndStream(error)) => {
                        if let Some(error) = error {
                            return Err(error);
                        }
                        break;
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }

            match stream.next().await {
                Some(Ok(chunk)) => reader.push(&chunk),
                Some(Err(error)) => {
                    let transport = self.map_transport_error(sandbox, error).await;
                    return Err(transport);
                }
                None => {
                    if reader.is_empty() {
                        break;
                    }
                    return Err(E2BError::Sandbox(
                        "truncated Connect envelope stream".into(),
                    ));
                }
            }
        }

        if !saw_exit {
            return Err(E2BError::Sandbox(
                "process stream ended without an exit event".into(),
            ));
        }
        Ok(E2BCommandResult {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code,
        })
    }
}

/// One parsed Connect envelope.
enum EnvelopeMessage {
    Data(Vec<u8>),
    EndStream(Option<E2BError>),
}

/// Incremental Connect envelope parser over a byte stream.
struct EnvelopeReader {
    buf: Vec<u8>,
}

impl EnvelopeReader {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Return the next complete envelope, or `None` while more bytes are
    /// needed. Callers must keep feeding chunks and retry.
    fn next_message(&mut self) -> Option<Result<EnvelopeMessage, E2BError>> {
        if self.buf.len() < 5 {
            return None;
        }
        let flags = self.buf[0];
        let length =
            u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if length > MAX_MESSAGE_BYTES {
            return Some(Err(E2BError::Sandbox(format!(
                "Connect envelope too large ({length} bytes)"
            ))));
        }
        if self.buf.len() < 5 + length {
            return None;
        }
        let payload: Vec<u8> = self.buf.drain(..5 + length).skip(5).collect();
        if flags & FLAG_COMPRESSED != 0 {
            return Some(Err(E2BError::Sandbox(
                "received an unsupported compressed Connect envelope".into(),
            )));
        }
        if flags & FLAG_END_STREAM != 0 {
            let parsed: Value = match serde_json::from_slice(&payload) {
                Ok(parsed) => parsed,
                Err(_) => {
                    return Some(Err(E2BError::Sandbox(
                        "malformed Connect end-stream envelope".into(),
                    )));
                }
            };
            let error = parsed
                .get("error")
                .filter(|error| !error.is_null())
                .map(error_from_json)
                .transpose()
                .map_err(|_| E2BError::Sandbox("malformed Connect error envelope".into()));
            return match error {
                Ok(error) => Some(Ok(EnvelopeMessage::EndStream(error))),
                Err(error) => Some(Err(error)),
            };
        }
        Some(Ok(EnvelopeMessage::Data(payload)))
    }
}

/// Decode a protobuf-JSON bytes field (standard base64, tolerant of padding)
/// to RAW bytes. Callers accumulate per stream and decode once at the end:
/// per-event lossy decoding split multi-byte UTF-8 characters across `data`
/// events into U+FFFD pairs (the BUG-46 class, never converted here).
fn decode_base64_field(data: &Value, field: &str) -> Result<Option<Vec<u8>>, E2BError> {
    let value = match data.get(field) {
        Some(Value::String(value)) => value,
        _ => return Ok(None),
    };
    decode_base64(value)
        .map(Some)
        .map_err(|_| E2BError::Sandbox(format!("invalid base64 in envd {field} event")))
}

fn decode_base64(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE};
    STANDARD
        .decode(value)
        .or_else(|_| STANDARD_NO_PAD.decode(value))
        .or_else(|_| URL_SAFE.decode(value))
}

/// Parse a Connect error JSON object (`{"code","message","details"}`).
fn error_from_json(value: &Value) -> Result<E2BError, ()> {
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown Connect error");
    Ok(map_connect_code(code, message))
}

/// Map an HTTP error from the envd data plane (Connect unary-error style).
fn map_connect_http_error(status: StatusCode, text: &str) -> E2BError {
    // Prefer the structured Connect error body when present.
    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
        if let Some(error) = parsed.get("error").filter(|error| !error.is_null()) {
            if let Ok(mapped) = error_from_json(error) {
                return mapped;
            }
        }
        if let Some(code) = parsed.get("code").and_then(Value::as_str) {
            return map_connect_code(code, text);
        }
    }
    match status {
        StatusCode::UNAUTHORIZED => E2BError::Auth(text.into()),
        StatusCode::NOT_FOUND => E2BError::NotFound(text.into()),
        StatusCode::TOO_MANY_REQUESTS => E2BError::RateLimit(text.into()),
        StatusCode::BAD_GATEWAY => E2BError::Timeout(text.into()),
        StatusCode::INSUFFICIENT_STORAGE => E2BError::NotEnoughSpace(text.into()),
        StatusCode::BAD_REQUEST => E2BError::InvalidInput(text.into()),
        _ => E2BError::Sandbox(format!("envd returned {status}: {text}")),
    }
}

/// Map a REST control-plane error status to the SDK error taxonomy.
fn map_rest_error(status: StatusCode, text: &str) -> E2BError {
    match status {
        StatusCode::BAD_REQUEST => E2BError::InvalidInput(text.into()),
        StatusCode::UNAUTHORIZED => E2BError::Auth(text.into()),
        StatusCode::NOT_FOUND => E2BError::NotFound(text.into()),
        StatusCode::TOO_MANY_REQUESTS => E2BError::RateLimit(text.into()),
        StatusCode::BAD_GATEWAY => E2BError::Timeout(text.into()),
        StatusCode::INSUFFICIENT_STORAGE => E2BError::NotEnoughSpace(text.into()),
        _ => E2BError::Sandbox(format!("E2B API returned {status}: {text}")),
    }
}

/// Map a Connect/gRPC code string to the SDK error taxonomy.
fn map_connect_code(code: &str, message: &str) -> E2BError {
    match code {
        "unauthenticated" | "permission_denied" => E2BError::Auth(message.into()),
        "not_found" => E2BError::NotFound(message.into()),
        "resource_exhausted" => E2BError::RateLimit(message.into()),
        "unavailable" | "deadline_exceeded" | "aborted" => E2BError::Timeout(message.into()),
        "invalid_argument" | "failed_precondition" | "out_of_range" => {
            E2BError::InvalidInput(message.into())
        }
        _ => E2BError::Sandbox(format!("{code}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvSnapshot(Vec<(&'static str, Option<String>)>);

    impl EnvSnapshot {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var(key).ok()))
                    .collect(),
            )
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn lock_env() -> (MutexGuard<'static, ()>, EnvSnapshot) {
        let guard = ENV_LOCK.lock().unwrap();
        let snapshot = EnvSnapshot::capture(&[
            "E2B_API_KEY",
            "E2B_API_URL",
            "E2B_SANDBOX_URL",
            "E2B_SANDBOX_TIMEOUT_MS",
        ]);
        (guard, snapshot)
    }

    fn envelope(flags: u8, payload: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(flags);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload.as_bytes());
        out
    }

    fn sandbox() -> E2BSandbox {
        E2BSandbox {
            sandbox_id: "sbx_test_123".into(),
            domain: None,
            envd_version: "0.7.0".into(),
            envd_access_token: "envd-token".into(),
            traffic_access_token: None,
        }
    }

    fn client_for(api_url: &str, sandbox_url: &str) -> E2BClient {
        E2BClient::new(E2BConfig {
            api_key: "key".into(),
            api_url: api_url.into(),
            sandbox_url_override: Some(sandbox_url.into()),
            domain: "e2b.app".into(),
            template: "base".into(),
            sandbox_timeout_secs: 300,
        })
    }

    struct MockRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    async fn read_http_request(stream: &mut TcpStream) -> MockRequest {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before headers");
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < header_end + 4 + content_length {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = buf[header_end + 4..header_end + 4 + content_length].to_vec();
        MockRequest {
            method,
            path,
            headers,
            body,
        }
    }

    async fn write_http_response(
        stream: &mut TcpStream,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        let reason = match status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            429 => "Too Many Requests",
            502 => "Bad Gateway",
            507 => "Insufficient Storage",
            _ => "Status",
        };
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (key, value) in headers {
            response.push_str(&format!("{key}: {value}\r\n"));
        }
        response.push_str("Connection: close\r\n\r\n");
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn http_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn(&MockRequest) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = read_http_request(&mut stream).await;
                let (status, headers, body) = handler(&request);
                write_http_response(&mut stream, status, &headers, &body).await;
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn create_sandbox_shapes_request_and_parses_response() {
        let (url, server) = http_server(|request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/sandboxes");
            assert_eq!(request.headers.get("x-api-key").unwrap(), "key");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["templateID"], "base");
            assert_eq!(body["timeout"], 300);
            assert_eq!(body["secure"], true);
            assert_eq!(body["allow_internet_access"], true);
            (
                201,
                vec![("Content-Type".into(), "application/json".into())],
                br#"{
                    "templateID": "base",
                    "sandboxID": "sbx_abc",
                    "clientID": "c1",
                    "envdVersion": "0.7.0",
                    "envdAccessToken": "envd-secret",
                    "trafficAccessToken": "traffic-secret",
                    "domain": "e2b.app"
                }"#
                .to_vec(),
            )
        })
        .await;
        let client = client_for(&url, &url);
        let sandbox = client.create_sandbox().await.unwrap();
        assert_eq!(sandbox.sandbox_id, "sbx_abc");
        assert_eq!(sandbox.envd_access_token, "envd-secret");
        assert_eq!(sandbox.envd_version, "0.7.0");
        assert_eq!(sandbox.domain.as_deref(), Some("e2b.app"));
        assert_eq!(
            sandbox.traffic_access_token.as_deref(),
            Some("traffic-secret")
        );
        server.abort();
    }

    #[tokio::test]
    async fn kill_sandbox_sends_delete_and_treats_404_as_gone() {
        let (url, server) = http_server(|request| {
            assert_eq!(request.method, "DELETE");
            assert_eq!(request.path, "/sandboxes/sbx_abc");
            assert_eq!(request.headers.get("x-api-key").unwrap(), "key");
            (204, vec![], Vec::new())
        })
        .await;
        let client = client_for(&url, &url);
        assert!(client.kill_sandbox("sbx_abc").await.unwrap());
        server.abort();

        let (url, server) = http_server(|_| (404, vec![], b"not found".to_vec())).await;
        let client = client_for(&url, &url);
        assert!(!client.kill_sandbox("sbx_abc").await.unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn create_maps_rest_errors() {
        for (status, expected) in [
            (401, "AUTH_FAILED"),
            (404, "NOT_FOUND"),
            (429, "RATE_LIMITED"),
            (502, "TIMEOUT"),
            (507, "NO_SPACE"),
            (400, "INVALID_INPUT"),
        ] {
            let (url, server) = http_server(move |_| (status, vec![], b"nope".to_vec())).await;
            let client = client_for(&url, &url);
            let error = client.create_sandbox().await.unwrap_err();
            assert_eq!(error.code(), expected, "status {status}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn run_command_parses_connect_stream() {
        let (url, server) = http_server(|request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/process.Process/Start");
            assert_eq!(
                request.headers.get("content-type").unwrap(),
                "application/connect+json"
            );
            assert_eq!(
                request.headers.get("connect-protocol-version").unwrap(),
                "1"
            );
            assert_eq!(request.headers.get("x-access-token").unwrap(), "envd-token");
            assert_eq!(
                request.headers.get("e2b-sandbox-id").unwrap(),
                "sbx_test_123"
            );
            assert_eq!(request.headers.get("e2b-sandbox-port").unwrap(), "49983");
            // Enveloped request body: flags 0 + BE length + JSON.
            assert_eq!(request.body[0], 0);
            let length = u32::from_be_bytes([
                request.body[1],
                request.body[2],
                request.body[3],
                request.body[4],
            ]) as usize;
            let payload = &request.body[5..5 + length];
            let parsed: Value = serde_json::from_slice(payload).unwrap();
            assert_eq!(parsed["process"]["cmd"], "/bin/bash");
            assert_eq!(parsed["process"]["args"], json!(["-l", "-c", "echo hi"]));
            assert_eq!(parsed["process"]["cwd"], "/home/user/workspace");
            assert_eq!(parsed["stdin"], false);

            use base64::engine::general_purpose::STANDARD;
            let mut body = Vec::new();
            body.extend(envelope(0, r#"{"event":{"start":{"pid":7}}}"#));
            body.extend(envelope(
                0,
                &format!(
                    r#"{{"event":{{"data":{{"stdout":"{}"}}}}}}"#,
                    STANDARD.encode(b"hello\n")
                ),
            ));
            body.extend(envelope(
                0,
                &format!(
                    r#"{{"event":{{"data":{{"stderr":"{}"}}}}}}"#,
                    STANDARD.encode(b"warn\n")
                ),
            ));
            body.extend(envelope(
                0,
                r#"{"event":{"end":{"exitCode":0,"exited":true}}}"#,
            ));
            body.extend(envelope(FLAG_END_STREAM, r#"{}"#));
            (
                200,
                vec![("Content-Type".into(), "application/connect+json".into())],
                body,
            )
        })
        .await;
        let client = client_for(&url, &url);
        let sandbox = sandbox();
        let mut envs = BTreeMap::new();
        envs.insert("FOO".into(), "bar".into());
        let result = client
            .run_command(&sandbox, "echo hi", Some("/home/user/workspace"), &envs)
            .await
            .unwrap();
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "warn\n");
        assert_eq!(result.exit_code, 0);
        server.abort();
    }

    #[tokio::test]
    async fn run_command_maps_end_stream_error() {
        let (url, server) = http_server(|_| {
            let mut body = Vec::new();
            body.extend(envelope(0, r#"{"event":{"start":{"pid":1}}}"#));
            body.extend(envelope(
                FLAG_END_STREAM,
                r#"{"error":{"code":"unavailable","message":"sandbox gone"}}"#,
            ));
            (200, vec![], body)
        })
        .await;
        let client = client_for(&url, &url);
        let error = client
            .run_command(&sandbox(), "true", None, &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(matches!(error, E2BError::Timeout(_)), "{error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn run_command_maps_http_error() {
        let (url, server) = http_server(|_| {
            (
                401,
                vec![("Content-Type".into(), "application/json".into())],
                br#"{"code":"unauthenticated","message":"bad token"}"#.to_vec(),
            )
        })
        .await;
        let client = client_for(&url, &url);
        let error = client
            .run_command(&sandbox(), "true", None, &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(matches!(error, E2BError::Auth(_)), "{error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn run_command_rejects_missing_exit_event() {
        let (url, server) = http_server(|_| {
            let mut body = Vec::new();
            body.extend(envelope(0, r#"{"event":{"start":{"pid":1}}}"#));
            body.extend(envelope(FLAG_END_STREAM, r#"{}"#));
            (200, vec![], body)
        })
        .await;
        let client = client_for(&url, &url);
        let error = client
            .run_command(&sandbox(), "true", None, &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(matches!(error, E2BError::Sandbox(_)), "{error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn health_probe_detects_dead_sandbox() {
        let (url, server) = http_server(|request| {
            assert_eq!(request.path, "/health");
            (502, vec![], Vec::new())
        })
        .await;
        let client = client_for(&url, &url);
        assert_eq!(client.check_health(&sandbox()).await.unwrap(), Some(false));
        server.abort();
    }

    #[tokio::test]
    async fn envd_url_uses_stable_host_for_known_domains() {
        let config = E2BConfig {
            api_key: "key".into(),
            api_url: "https://api.e2b.app".into(),
            sandbox_url_override: None,
            domain: "e2b.app".into(),
            template: "base".into(),
            sandbox_timeout_secs: 300,
        };
        assert_eq!(config.envd_url(&sandbox()), "https://sandbox.e2b.app");
        let custom = E2BSandbox {
            domain: Some("example.com".into()),
            ..sandbox()
        };
        assert_eq!(
            config.envd_url(&custom),
            "https://49983-sbx_test_123.example.com"
        );
    }

    #[tokio::test]
    async fn envelope_reader_handles_split_messages() {
        let mut reader = EnvelopeReader::new();
        let bytes = envelope(0, r#"{"event":{"start":{"pid":1}}}"#);
        assert!(reader.next_message().is_none());
        reader.push(&bytes[..3]);
        assert!(reader.next_message().is_none());
        reader.push(&bytes[3..7]);
        reader.push(&bytes[7..]);
        let message = reader.next_message().unwrap().unwrap();
        match message {
            EnvelopeMessage::Data(payload) => {
                let parsed: Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(parsed["event"]["start"]["pid"], 1);
            }
            _ => panic!("expected data envelope"),
        }
    }

    #[test]
    fn connect_code_mapping_covers_sdk_taxonomy() {
        assert!(matches!(
            map_connect_code("unauthenticated", "x"),
            E2BError::Auth(_)
        ));
        assert!(matches!(
            map_connect_code("not_found", "x"),
            E2BError::NotFound(_)
        ));
        assert!(matches!(
            map_connect_code("resource_exhausted", "x"),
            E2BError::RateLimit(_)
        ));
        assert!(matches!(
            map_connect_code("deadline_exceeded", "x"),
            E2BError::Timeout(_)
        ));
        assert!(matches!(
            map_connect_code("invalid_argument", "x"),
            E2BError::InvalidInput(_)
        ));
    }

    #[test]
    fn from_env_reports_unconfigured_without_key() {
        let (_guard, _env) = lock_env();
        std::env::remove_var("E2B_API_KEY");
        let error = E2BConfig::from_env().unwrap_err();
        assert!(matches!(error, E2BError::Unconfigured(_)));
    }

    /// BUG-65 regression (round 5): a sub-second E2B_SANDBOX_TIMEOUT_MS is an
    /// explicit configuration — silently widening it to the 300s default is
    /// the opposite direction of the operator's intent (sub-second must
    /// clamp down to at least 1s, never up).
    #[test]
    fn from_env_subsecond_timeout_clamps_down_not_up() {
        let (_guard, _env) = lock_env();
        std::env::set_var("E2B_API_KEY", "e2b_key");
        std::env::set_var("E2B_SANDBOX_TIMEOUT_MS", "500");
        let config = E2BConfig::from_env().unwrap();
        assert_eq!(
            config.sandbox_timeout_secs, 1,
            "500ms must become 1s (minimum), not the 300s default"
        );
    }

    #[test]
    fn from_env_reads_overrides() {
        let (_guard, _env) = lock_env();
        std::env::set_var("E2B_API_KEY", "e2b_key");
        std::env::set_var("E2B_API_URL", "https://api.example.test");
        std::env::set_var("E2B_SANDBOX_TIMEOUT_MS", "120000");
        let config = E2BConfig::from_env().unwrap();
        assert_eq!(config.api_key, "e2b_key");
        assert_eq!(config.api_url, "https://api.example.test");
        assert_eq!(config.sandbox_timeout_secs, 120);
    }
}
