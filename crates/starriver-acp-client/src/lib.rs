//! WI-112: a minimal ACP (Agent Client Protocol) **client** — spawns an
//! external ACP agent binary as a child process and drives one prompt to
//! completion over newline-delimited JSON-RPC on stdio. This is the reverse
//! half of `starriver-acp` (the server): together they let starriver both
//! BE driven by ACP clients and DRIVE other ACP agents as subagents.
//!
//! Wire shape mirrors the server side: client→agent `Request` frames
//! (`initialize`/`session/new`/`session/prompt`), agent→client `Response`
//! frames (id-correlated) interleaved with `session/update` notifications;
//! `session/request_permission` requests from the agent are answered by the
//! caller's trust policy.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How the agent's permission asks are answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Answer `allow_once` — the operator explicitly trusts this binary.
    Allow,
    /// Answer `reject` (default) — an untrusted agent gets no tool access.
    Reject,
}

/// One external ACP agent declaration (from settings).
#[derive(Debug, Clone)]
pub struct AcpAgentSpec {
    pub command: String,
    pub args: Vec<String>,
    /// Working directory for the child (the session cwd).
    pub cwd: Option<String>,
    pub permissions: PermissionPolicy,
}

#[derive(Debug, Clone)]
pub struct AcpRunResult {
    /// Concatenated agent message text across the run (chunks + finals).
    pub text: String,
    /// The prompt response's stopReason (`end_turn`, `cancelled`, …).
    pub stop_reason: String,
}

struct AgentProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl AgentProcess {
    fn spawn(spec: &AcpAgentSpec) -> Result<Self, String> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = spec.cwd.as_deref() {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawning ACP agent `{}`: {e}", spec.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "agent stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent stdout unavailable".to_string())?;
        // Reader thread: each stdout line → channel; EOF drops the sender.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let line = serde_json::to_string(message).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .map_err(|e| format!("writing to agent stdin: {e}"))
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        // Closing stdin (Drop of ChildStdin) is the polite shutdown signal;
        // wait briefly, then force-kill so a wedged agent can't leak.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Run one prompt against an external ACP agent. Blocks the calling thread
/// (call from `spawn_blocking` in async contexts); `timeout` bounds the
/// whole conversation so a wedged agent cannot pin the parent forever.
pub fn run_prompt(
    spec: &AcpAgentSpec,
    prompt: &str,
    timeout: Duration,
) -> Result<AcpRunResult, String> {
    let deadline = Instant::now() + timeout;
    let mut agent = AgentProcess::spawn(spec)?;

    // ── handshake ──
    agent.send(&request(
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    ))?;
    let initialize = read_until_response(&mut agent, 1, deadline)?;
    if initialize.get("error").is_some() {
        return Err(format!("agent initialize failed: {initialize}"));
    }
    agent.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))?;

    // ── session + prompt ──
    agent.send(&request(
        2,
        "session/new",
        json!({"cwd": spec.cwd.clone().unwrap_or_default()}),
    ))?;
    let new_session = read_until_response(&mut agent, 2, deadline)?;
    let session_id = new_session
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("agent session/new failed: {new_session}"))?
        .to_string();

    agent.send(&request(
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": {"content": [{"type": "text", "text": prompt}]}
        }),
    ))?;

    // ── pump: updates + permission asks until the prompt response ──
    let mut text = String::new();
    loop {
        let line = agent
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "timed out waiting for the agent".to_string())?;
        let message: Value =
            serde_json::from_str(&line).map_err(|e| format!("bad agent frame: {e}"))?;
        if message.get("id").is_some()
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            if message["id"].as_u64() == Some(3) {
                if let Some(error) = message.get("error") {
                    return Err(format!("agent prompt failed: {error}"));
                }
                let stop_reason = message
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string();
                return Ok(AcpRunResult {
                    text,
                    stop_reason,
                });
            }
            // Responses to other ids (none expected) are ignored.
            continue;
        }
        match message.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                let update = &message["params"]["update"];
                match update.get("type").and_then(Value::as_str) {
                    Some("agent_message_chunk") | Some("agent_message") => {
                        if let Some(chunk) = update
                            .pointer("/content/0/text")
                            .and_then(Value::as_str)
                        {
                            text.push_str(chunk);
                        }
                    }
                    _ => {}
                }
            }
            Some("session/request_permission") => {
                let id = message["id"].clone();
                let choice = match spec.permissions {
                    PermissionPolicy::Allow => "allow_once",
                    PermissionPolicy::Reject => "reject",
                };
                agent.send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"outcome": {"id": choice}}
                }))?;
            }
            _ => {}
        }
    }
}

/// Read frames until the response with `id` arrives (updates/permissions
/// interleaved on the way are consumed by the caller's pump — during the
/// handshake nothing else is expected, so plain draining is fine).
fn read_until_response(
    agent: &mut AgentProcess,
    id: u64,
    deadline: Instant,
) -> Result<Value, String> {
    loop {
        let line = agent
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "timed out waiting for the agent".to_string())?;
        let message: Value =
            serde_json::from_str(&line).map_err(|e| format!("bad agent frame: {e}"))?;
        if message["id"].as_u64() == Some(id)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return Ok(message);
        }
    }
}
