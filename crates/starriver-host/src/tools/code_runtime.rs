//! Code-execution tools (`code-runtime` / `e2b` parity, local first):
//! `python_exec` runs Python on the device; `e2b_run` runs a command inside a
//! remote E2B Linux sandbox through the REST control plane and the envd
//! Connect data plane (see [`crate::e2b`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::e2b::{E2BClient, E2BConfig, E2BError};

struct PythonExec;

impl ToolExecutor for PythonExec {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let code = json
                .get("code")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| invalid_input("python_exec", "missing code".into()))?;
            // timeout: 0 must mean "use the default", not an instant
            // timeout (the bash-tool convention).
            let timeout_secs = json
                .get("timeout")
                .and_then(|t| t.as_u64())
                .filter(|t| *t > 0)
                .unwrap_or(30);
            let cwd = ctx
                .execution
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let output = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                tokio::process::Command::new("python3")
                    .arg("-c")
                    .arg(code)
                    .current_dir(&cwd)
                    // The timeout drops the future; without kill_on_drop the
                    // python3 process keeps running detached (an infinite
                    // loop leaks one process per timed-out call).
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| {
                tool_failure(
                    "python_exec",
                    "TIMEOUT",
                    format!("python execution exceeded {timeout_secs}s"),
                )
            })?
            .map_err(|e| tool_failure("python_exec", "EXECUTION_FAILED", e.to_string()))?;
            let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.stderr.is_empty() {
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            if output.status.success() {
                Ok(Value::new(serde_json::json!({ "output": combined })))
            } else {
                Err(tool_failure(
                    "python_exec",
                    "EXECUTION_FAILED",
                    format!("exit {:?}\n{combined}", output.status.code()),
                ))
            }
        })
    }
}

struct E2BRun;

impl ToolExecutor for E2BRun {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let command = json
                .get("command")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| invalid_input("e2b_run", "missing command".into()))?
                .to_string();
            let cwd = json.get("cwd").and_then(|c| c.as_str()).map(String::from);
            let timeout_secs = json
                .get("timeout")
                .and_then(|t| t.as_u64())
                .filter(|t| *t > 0)
                .unwrap_or(60);
            let mut envs = BTreeMap::new();
            if let Some(object) = json.get("envs").and_then(|e| e.as_object()) {
                for (key, value) in object {
                    if let Some(value) = value.as_str() {
                        envs.insert(key.clone(), value.to_string());
                    }
                }
            }

            let config = E2BConfig::from_env()
                .map_err(|e| tool_failure("e2b_run", e.code(), e.to_string()))?;
            let client = E2BClient::new(config);
            let sandbox = client
                .create_sandbox()
                .await
                .map_err(|e| tool_failure("e2b_run", e.code(), e.to_string()))?;

            let run = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                client.run_command(&sandbox, &command, cwd.as_deref(), &envs),
            )
            .await;
            // Dispose the sandbox on every path; the API key never leaks into
            // the sandbox and the TTL is a backstop for a failed kill.
            let kill = client.kill_sandbox(&sandbox.sandbox_id).await;
            if let Err(error) = kill {
                tracing::warn!(?error, sandbox_id = %sandbox.sandbox_id, "e2b sandbox kill failed");
            }

            let result = match run {
                Err(_elapsed) => {
                    return Err(tool_failure(
                        "e2b_run",
                        "TIMEOUT",
                        format!("command exceeded {timeout_secs}s inside the remote sandbox"),
                    ));
                }
                Ok(result) => result.map_err(map_e2b_error)?,
            };
            let combined = if result.stderr.is_empty() {
                result.stdout
            } else {
                format!("{}\n[stderr]\n{}", result.stdout, result.stderr)
            };
            if result.exit_code == 0 {
                Ok(Value::new(serde_json::json!({ "output": combined })))
            } else {
                Err(tool_failure(
                    "e2b_run",
                    "EXECUTION_FAILED",
                    format!("exit {}\n{combined}", result.exit_code),
                ))
            }
        })
    }
}

/// Convert an E2B adapter error into a shell tool failure.
fn map_e2b_error(error: E2BError) -> starriver_kernel::tools::ToolError {
    tool_failure("e2b_run", error.code(), error.to_string())
}

pub fn python_exec_definition() -> ToolDefinition {
    ToolDefinition {
        name: "python_exec".into(),
        description:
            "Execute a Python snippet on the device (python3) and return its stdout/stderr. Use for quick computations and scripting; prefer bash for shell-level work."
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "code": { "type": "string" },
                "timeout": { "type": "integer" }
            },
            "required": ["code"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(PythonExec),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(60)),
        requires_approval: true,
        deny_in_readonly: false,
        finalize_content: None,
    }
}

pub fn e2b_run_definition() -> ToolDefinition {
    ToolDefinition {
        name: "e2b_run".into(),
        description:
            "Run a shell command inside a fresh remote E2B Linux sandbox (requires E2B_API_KEY). The sandbox is created for the call and disposed afterwards; use for code the device cannot safely run."
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string", "description": "Remote working directory (defaults to the sandbox user's home)" },
                "timeout": { "type": "integer", "description": "Command timeout in seconds (default 60)" },
                "envs": { "type": "object", "description": "Extra environment variables for the command" }
            },
            "required": ["command"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(E2BRun),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(120)),
        requires_approval: true,
        deny_in_readonly: false,
        finalize_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use starriver_kernel::tools::ToolExecution;
    use std::sync::MutexGuard;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restore E2B env vars to their captured values on drop, even on panic.
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
        let snapshot = EnvSnapshot::capture(&["E2B_API_KEY", "E2B_API_URL", "E2B_SANDBOX_URL"]);
        (guard, snapshot)
    }

    fn tool_context(command: serde_json::Value) -> ToolRunContext {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "e2b_run".into(),
            arguments: Value::new(command),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(execution)
    }

    fn envelope(flags: u8, payload: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(flags);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload.as_bytes());
        out
    }

    async fn read_request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
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
        let mut parts = lines.next().unwrap_or_default().split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let content_length: usize = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
            .find_map(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < header_end + 4 + content_length {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = buf[header_end + 4..header_end + 4 + content_length].to_vec();
        (method, path, body)
    }

    async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: Vec<u8>) {
        let reason = if status == 201 { "Created" } else { "OK" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn e2b_run_full_lifecycle() {
        let (_guard, _env) = lock_env();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (method, path, body) = read_request(&mut stream).await;
                seen_clone.lock().unwrap().push(format!("{method} {path}"));
                match (method.as_str(), path.as_str()) {
                    ("POST", "/sandboxes") => {
                        respond(
                            &mut stream,
                            201,
                            "application/json",
                            br#"{
                                "templateID": "base",
                                "sandboxID": "sbx_tool",
                                "clientID": "c1",
                                "envdVersion": "0.7.0",
                                "envdAccessToken": "envd-tok",
                                "domain": "e2b.app"
                            }"#
                            .to_vec(),
                        )
                        .await;
                    }
                    ("POST", "/process.Process/Start") => {
                        use base64::engine::general_purpose::STANDARD;
                        let mut out = Vec::new();
                        out.extend(envelope(0, r#"{"event":{"start":{"pid":1}}}"#));
                        out.extend(envelope(
                            0,
                            &format!(
                                r#"{{"event":{{"data":{{"stdout":"{}"}}}}}}"#,
                                STANDARD.encode(b"hi from sandbox\n")
                            ),
                        ));
                        out.extend(envelope(
                            0,
                            r#"{"event":{"end":{"exitCode":0,"exited":true}}}"#,
                        ));
                        out.extend(envelope(0x02, r#"{}"#));
                        respond(&mut stream, 200, "application/connect+json", out).await;
                    }
                    ("DELETE", "/sandboxes/sbx_tool") => {
                        respond(&mut stream, 204, "text/plain", Vec::new()).await;
                    }
                    _ => {
                        let _ = body;
                        respond(&mut stream, 404, "text/plain", b"nope".to_vec()).await;
                    }
                }
            }
        });

        std::env::set_var("E2B_API_KEY", "key");
        std::env::set_var("E2B_API_URL", &url);
        std::env::set_var("E2B_SANDBOX_URL", &url);

        let result = E2BRun
            .execute(
                Value::new(serde_json::json!({ "command": "echo hi" })),
                tool_context(serde_json::json!({ "command": "echo hi" })),
            )
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert_eq!(json["output"], "hi from sandbox\n");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "POST /sandboxes",
                "POST /process.Process/Start",
                "DELETE /sandboxes/sbx_tool"
            ]
        );
        server.abort();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn e2b_run_reports_unconfigured_without_key() {
        let (_guard, _env) = lock_env();
        std::env::remove_var("E2B_API_KEY");
        let error = E2BRun
            .execute(
                Value::new(serde_json::json!({ "command": "echo hi" })),
                tool_context(serde_json::json!({ "command": "echo hi" })),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "UNCONFIGURED");
    }
}
