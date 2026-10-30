//! Persistent-terminal model tools (`terminal` six-tool set parity):
//! start / write / read / wait / list / kill plus `terminal_signal` over the
//! host terminal service. Sends report wait reasons
//! (`stdin_read`/`inferred_idle`/`timeout`/`session_exit`) and the
//! top-level session status; reads are non-destructive newest-relative
//! pages over the retained scrollback.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::terminal::{TerminalService, DEFAULT_IDLE_MS, DEFAULT_WAIT_MS};

fn terminal_failure(action: &str, message: String) -> ToolError {
    tool_failure("terminal", action, message)
}

fn required_id(json: &serde_json::Value) -> Result<String, ToolError> {
    json.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| invalid_input("terminal", "missing id".into()))
}

/// Run the blocking send off the async worker when a tokio runtime is
/// available, else on a plain thread (keeps sync executors and tests working
/// under any executor).
async fn run_send(
    service: Arc<TerminalService>,
    id: String,
    input: Option<String>,
    wait_ms: u64,
    idle_ms: u64,
) -> Result<crate::terminal::SendOutcome, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(move || {
            service.send_ex(&id, input.as_deref(), wait_ms, idle_ms, true)
        })
        .await
        .map_err(|e| format!("send join: {e}"))?
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(service.send_ex(&id, input.as_deref(), wait_ms, idle_ms, true));
        });
        rx.recv().map_err(|e| format!("send join: {e}"))?
    }
}

struct TerminalStart {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalStart {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let shell = json
                .get("shell")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            // dsh 式归属：owner/cwd 来自执行上下文（agent_id 即 session id，
            // cwd 即会话工作区根），模型无法伪造。
            let owner = ctx.execution.agent_id.clone();
            let cwd = ctx
                .execution
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            let id = service
                .start_owned(owner, cwd, shell)
                .map_err(|e| terminal_failure("START_FAILED", e))?;
            Ok(Value::new(serde_json::json!({
                "output": format!("terminal started: {id}")
            })))
        })
    }
}

struct TerminalWrite {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalWrite {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let id = required_id(&json)?;
            let input = json
                .get("input")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid_input("terminal_write", "missing input".into()))?;
            match json.get("waitMs").and_then(|v| v.as_u64()) {
                // send semantics: write + readiness wait + wait reason.
                Some(wait_ms) => {
                    let idle_ms = json
                        .get("idleMs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(DEFAULT_IDLE_MS);
                    let outcome = run_send(
                        Arc::clone(&service),
                        id.clone(),
                        Some(input.to_string()),
                        wait_ms,
                        idle_ms,
                    )
                    .await
                    .map_err(|e| terminal_failure("WRITE_FAILED", e))?;
                    Ok(Value::new(serde_json::json!({
                        "output": outcome.output,
                        "waitReason": outcome.wait_reason.as_str(),
                        "sessionStatus": outcome.session_status.to_json(),
                        "truncated": outcome.truncated,
                    })))
                }
                None => {
                    service
                        .write(&id, input)
                        .map_err(|e| terminal_failure("WRITE_FAILED", e))?;
                    Ok(Value::new(serde_json::json!({ "output": "written" })))
                }
            }
        })
    }
}

struct TerminalRead {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalRead {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let id = required_id(&json)?;
            let offset = json.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let lines = json
                .get("lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .clamp(1, 2_000) as usize;
            let max_bytes = json
                .get("maxBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(8 * 1024) as usize;
            let (mut page, total, older) = service
                .read_page(&id, offset, lines)
                .map_err(|e| terminal_failure("READ_FAILED", e))?;
            // Byte bound trims the OLDEST end of the page (keep the newest).
            let mut truncated = older;
            if page.len() > max_bytes {
                let mut cut = page.len() - max_bytes;
                while cut < page.len() && !page.is_char_boundary(cut) {
                    cut += 1;
                }
                page.drain(..cut);
                truncated = true;
            }
            Ok(Value::new(serde_json::json!({
                "output": page,
                "totalLines": total,
                "truncated": truncated,
            })))
        })
    }
}

struct TerminalWait {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalWait {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let id = required_id(&json)?;
            let timeout_ms = json
                .get("timeoutMs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_WAIT_MS);
            // timeout is a wait reason reported to the model, not
            // a tool error — silence may simply mean a long-running command.
            let outcome = run_send(
                Arc::clone(&service),
                id.clone(),
                None,
                timeout_ms,
                DEFAULT_IDLE_MS,
            )
            .await
            .map_err(|e| terminal_failure("WAIT_FAILED", e))?;
            Ok(Value::new(serde_json::json!({
                "output": outcome.output,
                "waitReason": outcome.wait_reason.as_str(),
                "sessionStatus": outcome.session_status.to_json(),
                "truncated": outcome.truncated,
            })))
        })
    }
}

struct TerminalSignal {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalSignal {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let id = required_id(&json)?;
            let signal = json
                .get("signal")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid_input("terminal_signal", "missing signal".into()))?;
            let delivered = service
                .signal(&id, signal)
                .map_err(|e| terminal_failure("SIGNAL_FAILED", e))?;
            Ok(Value::new(serde_json::json!({ "output": delivered })))
        })
    }
}

struct TerminalList {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalList {
    fn execute(
        &self,
        _arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            // dsh 授权语义：模型只看到本会话的终端——别的会话的 id 连出现
            // 都不出现，kill 也就无从伪造。
            let owner = ctx.execution.agent_id.clone();
            let sessions: Vec<_> = service
                .list()
                .into_iter()
                .filter(|t| {
                    owner
                        .as_deref()
                        .is_none_or(|o| t.owner.as_deref() == Some(o))
                })
                .collect();
            let output = if sessions.is_empty() {
                "no terminals".into()
            } else {
                sessions
                    .iter()
                    .map(
                        |t| match t.command.as_ref().and_then(|c| c.cmd.as_deref()) {
                            Some(cmd) => {
                                format!("{} ({}) — {}", t.id, t.status, cmd.replace('\n', " "))
                            }
                            None => format!("{} ({})", t.id, t.status),
                        },
                    )
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

struct TerminalKill {
    service: Arc<TerminalService>,
}

impl ToolExecutor for TerminalKill {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let id = required_id(&json)?;
            service
                .kill(&id, ctx.execution.agent_id.as_deref())
                .map_err(|e| terminal_failure("KILL_FAILED", e))?;
            Ok(Value::new(serde_json::json!({ "output": "terminated" })))
        })
    }
}

/// tool-guide parity (`tool:pty` @ 1700 → `terminal:guide` @ 160): one
/// section for the whole persistent-terminal family.
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "terminal",
        160,
        "Use terminal sessions only when work needs persistent state or interactive stdin (dev servers, REPLs, prompts); prefer bash, read, write and edit for bounded one-shot operations. Track every session id and kill sessions that no longer matter.",
    )
}

/// Register the persistent-terminal tools, all sharing one service.
pub fn terminal_definitions(service: Arc<TerminalService>) -> Vec<ToolDefinition> {
    let mut defs = Vec::new();
    defs.push(ToolDefinition {
        name: "terminal_start".into(),
        description: "Start a persistent interactive terminal (PTY) session and return its id; the session survives across tool calls until killed.".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "shell": { "type": "string", "description": "shell binary (default bash)" } }
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalStart { service: Arc::clone(&service) }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(10)),
        requires_approval: true,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_write".into(),
        description: "Write input (e.g. a command plus newline) to a persistent terminal by id. \
                      Optionally set waitMs to wait for readiness and get the produced output \
                      together with waitReason (stdin_read | inferred_idle | timeout | \
                      session_exit) and the session status."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "input": { "type": "string" },
                "waitMs": { "type": "integer", "description": "wait budget for readiness (default: no wait)" },
                "idleMs": { "type": "integer", "description": "quiet window that ends the wait (default 400)" }
            },
            "required": ["id", "input"]
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalWrite {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(150)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_read".into(),
        description: "Read retained output of a persistent terminal by id (non-destructive \
                      newest-relative page over the scrollback). Returns output, totalLines and \
                      truncated."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "offset": { "type": "integer", "description": "lines back from the newest (default 0)" },
                "lines": { "type": "integer", "description": "max lines to return (default 200)" },
                "maxBytes": { "type": "integer", "description": "byte bound on the returned page (default 8192)" }
            },
            "required": ["id"]
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalRead {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(10)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_wait".into(),
        description: "Wait for new output of a persistent terminal (readiness inference: quiet \
                      window, budget, or shell exit). Returns output plus waitReason \
                      (stdin_read | inferred_idle | timeout | session_exit) and sessionStatus; \
                      a timeout is reported as a reason, not an error."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "timeoutMs": { "type": "integer", "description": "wait budget (default 5000)" }
            },
            "required": ["id"]
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalWait {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(150)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_signal".into(),
        description: "Forward a signal to a persistent terminal: SIGINT/SIGQUIT/SIGTSTP reach \
                      the foreground process group via the PTY; SIGTERM/SIGKILL terminate the \
                      top-level shell."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "signal": { "type": "string", "description": "SIGINT | SIGQUIT | SIGTSTP | SIGTERM | SIGKILL" }
            },
            "required": ["id", "signal"]
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalSignal {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(10)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_list".into(),
        description: "List live persistent terminal sessions with their top-level status.".into(),
        parameters: Value::new(serde_json::json!({ "type": "object", "properties": {} })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalList {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: Some(Arc::new(|_| true)),
        timeout: Some(Duration::from_secs(10)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs.push(ToolDefinition {
        name: "terminal_kill".into(),
        description: "Terminate a persistent terminal session by id.".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        })),
        output: Value::new(serde_json::json!({ "type": "object" })),
        render: Arc::new(text_content),
        executor: Arc::new(TerminalKill {
            service: Arc::clone(&service),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(10)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    });
    defs
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use starriver_kernel::tools::{mint_tool_execution_token, ToolExecution, ToolExecutor as _};

    fn ctx(name: &str) -> ToolRunContext {
        let exec = ToolExecution {
            call_id: starriver_kernel::llm::CallId(name.into()),
            root_call_id: starriver_kernel::llm::CallId(name.into()),
            name: name.into(),
            arguments: Value::new(()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(exec)
    }

    fn by_name(defs: &[ToolDefinition], name: &str) -> Arc<dyn ToolExecutor> {
        defs.iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("tool {name} missing"))
            .executor
            .clone()
    }

    fn start(service: &Arc<TerminalService>, shell: Option<&str>) -> String {
        let defs = terminal_definitions(Arc::clone(service));
        let args = match shell {
            Some(shell) => serde_json::json!({ "shell": shell }),
            None => serde_json::json!({}),
        };
        let out = block_on(
            by_name(&defs, "terminal_start").execute(Value::new(args), ctx("terminal_start")),
        )
        .expect("start");
        let json = out.downcast::<serde_json::Value>().unwrap();
        let text = json["output"].as_str().unwrap();
        text.strip_prefix("terminal started: ")
            .map(str::to_string)
            .unwrap_or_else(|| panic!("unexpected start output: {text}"))
    }

    fn payload(result: Result<Value, ToolError>) -> serde_json::Value {
        result
            .unwrap()
            .downcast::<serde_json::Value>()
            .map(|arc| (*arc).clone())
            .unwrap_or_else(|_| serde_json::json!({}))
    }

    #[test]
    fn full_lifecycle_via_executors() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));
        // Explicit `sh`: this test drives the executor plumbing with POSIX
        // commands — the platform default shell (PowerShell on Windows)
        // brings a slow banner/init phase that defeats the tight idle
        // budgets. Default-shell selection is covered by start().
        let id = start(&service, Some("sh"));
        assert!(!id.is_empty());

        // waitMs send: output + readiness reasoning in one call.
        let out = payload(block_on(by_name(&defs, "terminal_write").execute(
            Value::new(serde_json::json!({
                "id": id, "input": "echo terminal-e2e\n", "waitMs": 5000, "idleMs": 300
            })),
            ctx("terminal_write"),
        )));
        assert_eq!(out["waitReason"], "inferred_idle", "{out}");
        assert!(
            out["output"].as_str().unwrap().contains("terminal-e2e"),
            "{out}"
        );
        assert_eq!(out["sessionStatus"]["kind"], "running");

        // Non-destructive paged read still sees the history.
        let read = payload(block_on(by_name(&defs, "terminal_read").execute(
            Value::new(serde_json::json!({ "id": id })),
            ctx("terminal_read"),
        )));
        assert!(
            read["output"].as_str().unwrap().contains("terminal-e2e"),
            "{read}"
        );

        let listed = payload(block_on(
            by_name(&defs, "terminal_list")
                .execute(Value::new(serde_json::json!({})), ctx("terminal_list")),
        ));
        let listed_text = listed["output"].as_str().unwrap();
        assert!(
            listed_text.contains(&id),
            "{listed_json_text}",
            listed_json_text = listed_text
        );
        assert!(listed_text.contains("running"), "{listed_text}");

        block_on(by_name(&defs, "terminal_kill").execute(
            Value::new(serde_json::json!({ "id": id })),
            ctx("terminal_kill"),
        ))
        .unwrap();
        assert!(service.list().is_empty(), "session must be removed");
    }

    #[test]
    fn start_with_explicit_shell() {
        let service = Arc::new(TerminalService::new());
        let id = start(&service, Some("sh"));
        assert!(!id.is_empty());
        service.kill(&id, None).unwrap();
    }

    #[test]
    fn missing_arguments_are_invalid_input() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));

        let err = block_on(by_name(&defs, "terminal_write").execute(
            Value::new(serde_json::json!({ "input": "x" })),
            ctx("terminal_write"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");

        let err = block_on(by_name(&defs, "terminal_write").execute(
            Value::new(serde_json::json!({ "id": "nope" })),
            ctx("terminal_write"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");

        let err = block_on(
            by_name(&defs, "terminal_read")
                .execute(Value::new(serde_json::json!({})), ctx("terminal_read")),
        )
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");

        let err = block_on(
            by_name(&defs, "terminal_kill")
                .execute(Value::new(serde_json::json!({})), ctx("terminal_kill")),
        )
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");

        let err = block_on(by_name(&defs, "terminal_signal").execute(
            Value::new(serde_json::json!({ "id": "x" })),
            ctx("terminal_signal"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    #[test]
    fn operations_on_unknown_session_fail_with_stable_codes() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));

        let err = block_on(by_name(&defs, "terminal_read").execute(
            Value::new(serde_json::json!({ "id": "missing" })),
            ctx("terminal_read"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "READ_FAILED");

        let err = block_on(by_name(&defs, "terminal_write").execute(
            Value::new(serde_json::json!({ "id": "missing", "input": "x" })),
            ctx("terminal_write"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "WRITE_FAILED");

        let err = block_on(by_name(&defs, "terminal_kill").execute(
            Value::new(serde_json::json!({ "id": "missing" })),
            ctx("terminal_kill"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "KILL_FAILED");

        let err = block_on(by_name(&defs, "terminal_signal").execute(
            Value::new(serde_json::json!({ "id": "missing", "signal": "SIGINT" })),
            ctx("terminal_signal"),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "SIGNAL_FAILED");
    }

    #[tokio::test]
    async fn wait_reports_timeout_as_a_reason_not_an_error() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));
        let id = start(&service, Some("sh"));
        // Consume the prompt/echo of setup, then start a silent sleeper.
        let _ = payload(block_on(by_name(&defs, "terminal_write").execute(
            Value::new(
                serde_json::json!({ "id": id, "input": "true\n", "waitMs": 3000, "idleMs": 250 }),
            ),
            ctx("terminal_write"),
        )));
        let out = payload(
            by_name(&defs, "terminal_wait")
                .execute(
                    Value::new(serde_json::json!({
                        "id": id,
                        "timeoutMs": 250
                    })),
                    ctx("terminal_wait"),
                )
                .await,
        );
        // A quiet terminal times out as a REASON with the shell still running.
        assert_eq!(out["waitReason"], "timeout", "{out}");
        assert_eq!(out["sessionStatus"]["kind"], "running");
        service.kill(&id, None).unwrap();
    }

    #[tokio::test]
    async fn wait_consumes_output_produced_after_an_earlier_write() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));
        let id = start(&service, Some("sh"));

        // Fire-and-forget write (legacy shape): the wait's consume cursor
        // still starts before the output, so the model never loses it.
        block_on(by_name(&defs, "terminal_write").execute(
            Value::new(serde_json::json!({ "id": id, "input": "echo wait-sees-me\n" })),
            ctx("terminal_write"),
        ))
        .unwrap();

        let out = payload(
            by_name(&defs, "terminal_wait")
                .execute(
                    Value::new(serde_json::json!({ "id": id, "timeoutMs": 5000 })),
                    ctx("terminal_wait"),
                )
                .await,
        );
        assert!(
            out["output"].as_str().unwrap().contains("wait-sees-me"),
            "{out}"
        );
        assert_eq!(out["waitReason"], "inferred_idle", "{out}");
        service.kill(&id, None).unwrap();
    }

    #[tokio::test]
    async fn signal_forwards_sigint_to_the_foreground_process() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));
        let id = start(&service, Some("sh"));
        let _ = payload(block_on(
            by_name(&defs, "terminal_write").execute(
                Value::new(serde_json::json!({ "id": id, "input": "sleep 60\n", "waitMs": 2000, "idleMs": 250 })),
                ctx("terminal_write"),
            ),
        ));
        let out = payload(
            by_name(&defs, "terminal_signal")
                .execute(
                    Value::new(serde_json::json!({ "id": id, "signal": "SIGINT" })),
                    ctx("terminal_signal"),
                )
                .await,
        );
        assert!(
            out["output"]
                .as_str()
                .unwrap()
                .contains("foreground process group"),
            "{out}"
        );
        service.kill(&id, None).unwrap();
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let service = Arc::new(TerminalService::new());
        let defs = terminal_definitions(Arc::clone(&service));
        let err = block_on(
            by_name(&defs, "terminal_start")
                .execute(Value::new(serde_json::json!("boom")), ctx("terminal_start")),
        )
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }
}
