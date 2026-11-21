//! Direct tool invocation — plugin platform P1 (design/plugin-platform.md §3.1).
//!
//! `tools.call` runs one registered tool through the FULL ToolRuntime pipeline
//! (pre-execute policy, guards, sandbox, post-execute, content finalization)
//! without creating a session, turn, or LLM call. This is the channel plugin
//! UIs use for deterministic data operations instead of spawning host sessions
//! through `session.execute`.
//!
//! Contract:
//! - `sessionId` is optional and only BORROWS context (the session's immutable
//!   cwd + folded sandbox mode). It never occupies the session's input slot,
//!   so a direct call can run alongside a live turn.
//! - Without `sessionId` the host default workspace and the workspace-write
//!   sandbox apply — never an unconstrained default.
//! - Approval-required tools are refused fail-closed (`APPROVAL_REQUIRED`)
//!   unless the borrowed context is danger-full-access (mirroring the
//!   pre-execute trigger, which never asks under that mode). The approval
//!   bridge routes through a live agent's session log; a direct call has no
//!   agent, so approval-gated tools stay on the session channel. No silent
//!   bypass.
//! - Unknown tools, guard denials, execution failures and timeouts surface as
//!   real errors — never a fabricated success.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use starriver_kernel::llm::CallId;
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::session::SessionId;
use starriver_kernel::tools::{ToolExecutionInput, ToolExecutionResult, ToolRuntime};
use tokio_util::sync::CancellationToken;

use crate::HostRuntime;

/// Default budget for one direct call.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard cap — a direct call is a data operation, not a turn.
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Model-facing failure of one direct call (mirrors `ToolFailure`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallError {
    pub message: String,
    pub code: String,
}

/// Result of one direct call — same shape as a settled `tool/result`:
/// canonical `value`, model-facing `content` blocks (wire JSON), and the
/// tool's presentation `meta` when it declares a presenter.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub content: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolCallError>,
}

impl HostRuntime {
    /// Run one registered tool directly (no session, no turn, no LLM).
    pub fn call_tool(
        &self,
        name: String,
        arguments: Value,
        session_id: Option<String>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<ToolCallOutcome> {
        // Resolve the execution context under the inner lock, then RELEASE it
        // before the (potentially long) body runs — tool bodies must never run
        // under the runtime lock, and the Arc keeps the kernel alive past the
        // guard's drop.
        let (agent, cwd, sandbox_mode) = {
            let inner = self.inner.blocking_lock();
            let inner = inner
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime not initialized"))?;
            let (cwd, sandbox_mode) = match session_id.as_deref() {
                Some(sid) => {
                    let inspection = inner
                        .persistence
                        .coordinator()
                        .load(SessionId(sid.to_string()))
                        .map_err(|e| anyhow::anyhow!("SESSION_NOT_FOUND: {sid}: {e}"))?;
                    let cwd = inspection.header.cwd.clone().map(std::path::PathBuf::from);
                    // Same last-wins fold the kernel applies to a live session
                    // (sandbox::effective_sandbox_mode), over the durable view;
                    // an unset mode falls back to the safe workspace-write default.
                    let mode = inspection
                        .events
                        .iter()
                        .rev()
                        .find(|event| event.event_type == "sandbox/mode")
                        .and_then(|event| event.data.as_str_at("mode"))
                        .and_then(SandboxMode::parse)
                        .unwrap_or(SandboxMode::WorkspaceWrite);
                    (cwd, mode)
                }
                None => (
                    self.default_tool_call_cwd(),
                    SandboxMode::WorkspaceWrite,
                ),
            };
            // Fail-closed approval gate, mirroring the pre-execute trigger:
            // danger-full-access never asks, everything else asks — and a
            // direct call has no agent to answer, so refuse up front with a
            // crisp contract error instead of a mid-pipeline denial.
            if sandbox_mode != SandboxMode::DangerFullAccess
                && inner.agent.tools().requires_approval(&name)
            {
                return Err(anyhow::anyhow!(
                    "APPROVAL_REQUIRED: tool \"{name}\" requires approval; tools.call runs \
                     non-approval tools only — run it through a session turn instead"
                ));
            }
            (Arc::clone(&inner.agent), cwd, sandbox_mode)
        };

        let timeout = Duration::from_millis(
            timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let token = CancellationToken::new();
        let input = ToolExecutionInput {
            call_id: CallId(format!("direct-{}", uuid::Uuid::new_v4())),
            root_call_id: None,
            name: name.clone(),
            // The tool seam keeps the lossless JSON tree at runtime; the wire
            // serde value crosses through JsonValue (infallible both ways).
            arguments: starriver_kernel::json::JsonValue::from_serde_json(arguments).to_value(),
            // No agent: the call belongs to no session turn. Approval routing
            // and agent-scoped restrictions deliberately do not apply.
            agent_id: None,
            cwd,
            sandbox_mode: Some(sandbox_mode),
            parent: None,
            cancellation: Some(token.clone()),
        };
        // presentation_meta only reads name/arguments/root_call_id; keep a
        // clone because execute consumes the input.
        let meta_input = input.clone();
        let handle = self.runtime_handle.clone();
        handle.block_on(async move {
            let tools = agent.tools();
            match tokio::time::timeout(timeout, tools.execute(input)).await {
                Err(_elapsed) => {
                    token.cancel();
                    Err(anyhow::anyhow!(
                        "TIMEOUT: tool \"{name}\" exceeded {}ms",
                        timeout.as_millis()
                    ))
                }
                Ok(Err(e)) => Err(anyhow::anyhow!("{}: {e}", e.code())),
                Ok(Ok(result)) => {
                    let content = result
                        .content()
                        .iter()
                        .map(|block| {
                            block
                                .to_json()
                                .and_then(|tree| tree.to_serde_json())
                                .map_err(|e| anyhow::anyhow!("content serialization: {e}"))
                        })
                        .collect::<anyhow::Result<Vec<Value>>>()?;
                    match result {
                        ToolExecutionResult::Success { value, .. } => {
                            let meta = tools.presentation_meta(&meta_input, &value);
                            let value = runtime_value_to_serde(&value)
                                .map_err(|e| anyhow::anyhow!("value serialization: {e}"))?;
                            Ok(ToolCallOutcome {
                                ok: true,
                                value: Some(value),
                                content,
                                meta,
                                error: None,
                            })
                        }
                        ToolExecutionResult::Failure { error, .. } => Ok(ToolCallOutcome {
                            ok: false,
                            value: None,
                            content,
                            meta: None,
                            error: Some(ToolCallError {
                                message: error.message,
                                code: error.code,
                            }),
                        }),
                    }
                }
            }
        })
    }

    /// Default workspace for a session-less direct call: the coding agent's
    /// active workspace binding, falling back to the process cwd.
    fn default_tool_call_cwd(&self) -> Option<std::path::PathBuf> {
        self.resolve_agent_config(crate::CODING_AGENT_NAME)
            .working_dir
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
    }
}

/// Canonical tool value → wire serde JSON. Tool bodies wrap EITHER a raw
/// `serde_json::Value` (the builtin convention, e.g. glob/grep) OR a lossless
/// `JsonValue` tree; read both, same dual path as the approval grader
/// (agent-loop approval.rs `argument_str`).
fn runtime_value_to_serde(
    value: &cordis_rslite::Value,
) -> Result<Value, starriver_kernel::json::JsonValueError> {
    if let Ok(raw) = value.downcast::<serde_json::Value>() {
        return Ok(raw.as_ref().clone());
    }
    starriver_kernel::json::JsonValue::from_value(value).and_then(|tree| tree.to_serde_json())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::with_temp_home;
    use starriver_kernel::llm::ContentBlock;
    use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
    use starriver_tools::InMemoryToolRuntime;

    /// Minimal echo tool: returns its arguments verbatim.
    fn echo_definition(name: &str, requires_approval: bool) -> ToolDefinition {
        struct Echo;
        impl ToolExecutor for Echo {
            fn execute(
                &self,
                arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                Box::pin(async move { Ok(arguments) })
            }
        }
        ToolDefinition {
            name: name.into(),
            description: "test echo".into(),
            parameters: starriver_kernel::json::JsonValue::from_serde_json(
                serde_json::json!({ "type": "object" }),
            )
            .to_value(),
            output: starriver_kernel::json::JsonValue::from_serde_json(serde_json::json!({}))
                .to_value(),
            requires_approval,
            deny_in_readonly: false,
            render: Arc::new(|_args, value| {
                let text = starriver_kernel::json::JsonValue::from_value(value)
                    .ok()
                    .and_then(|tree| tree.to_serde_json().ok())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                vec![ContentBlock::Text(text)]
            }),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    fn register(host: &HostRuntime, definition: ToolDefinition) {
        let inner = host.inner.blocking_lock();
        let inner = inner.as_ref().expect("runtime initialized");
        // The registration lives as long as the test host; forget() declares
        // that intent instead of dropping the handle with a disposal warning.
        inner
            .agent
            .tools()
            .register(definition)
            .expect("register")
            .forget();
    }

    #[test]
    fn direct_call_runs_registered_tool_without_session() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            register(&host, echo_definition("p1_echo", false));
            let out = host
                .call_tool(
                    "p1_echo".into(),
                    serde_json::json!({ "q": "agent skills" }),
                    None,
                    None,
                )
                .expect("direct call succeeds");
            assert!(out.ok);
            assert_eq!(out.value.unwrap()["q"], "agent skills");
            assert!(!out.content.is_empty());
            assert!(out.error.is_none());
            // No session was created by the call.
            assert!(host.list_sessions(None).is_empty());
        });
    }

    /// Regression: builtin tools wrap a RAW serde_json::Value in the runtime
    /// Value (glob/grep convention) — the outcome serializer must read that
    /// shape, not only the lossless JsonValue tree.
    #[test]
    fn direct_call_serializes_raw_serde_tool_values() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            struct RawSerde;
            impl ToolExecutor for RawSerde {
                fn execute(
                    &self,
                    _arguments: cordis_rslite::Value,
                    _ctx: ToolRunContext,
                ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                    Box::pin(async move {
                        Ok(cordis_rslite::Value::new(serde_json::json!({ "output": "raw" })))
                    })
                }
            }
            let mut def = echo_definition("p1_raw", false);
            def.executor = Arc::new(RawSerde);
            register(&host, def);
            let out = host
                .call_tool("p1_raw".into(), serde_json::json!({}), None, None)
                .expect("raw serde value must serialize");
            assert!(out.ok);
            assert_eq!(out.value.unwrap()["output"], "raw");
        });
    }

    #[test]
    fn approval_required_tool_is_refused_fail_closed() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            register(&host, echo_definition("p1_gated", true));
            let err = host
                .call_tool("p1_gated".into(), serde_json::json!({}), None, None)
                .expect_err("approval-gated tool must be refused");
            assert!(err.to_string().contains("APPROVAL_REQUIRED"), "{err}");
        });
    }

    #[test]
    fn unknown_tool_is_a_real_error() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let out = host
                .call_tool("p1_nonexistent".into(), serde_json::json!({}), None, None)
                .expect("pipeline settles unknown tools as a failure result");
            assert!(!out.ok);
            assert!(out.error.is_some());
        });
    }

    #[test]
    fn unknown_session_context_is_a_real_error() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            register(&host, echo_definition("p1_echo2", false));
            let err = host
                .call_tool(
                    "p1_echo2".into(),
                    serde_json::json!({}),
                    Some("no-such-session".into()),
                    None,
                )
                .expect_err("unknown session must be refused");
            assert!(err.to_string().contains("SESSION_NOT_FOUND"), "{err}");
        });
    }

    #[test]
    fn timeout_cancels_and_reports() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            struct Slow;
            impl ToolExecutor for Slow {
                fn execute(
                    &self,
                    _arguments: cordis_rslite::Value,
                    ctx: ToolRunContext,
                ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                    Box::pin(async move {
                        // Honor cooperative cancellation so the timeout path
                        // settles promptly instead of sleeping the full budget.
                        if let Some(token) = ctx.execution.cancellation.clone() {
                            token.cancelled().await;
                        }
                        Ok(starriver_kernel::json::JsonValue::from_serde_json(
                            serde_json::json!({}),
                        )
                        .to_value())
                    })
                }
            }
            let mut def = echo_definition("p1_slow", false);
            def.executor = Arc::new(Slow);
            register(&host, def);
            let err = host
                .call_tool("p1_slow".into(), serde_json::json!({}), None, Some(50))
                .expect_err("slow tool must time out");
            assert!(err.to_string().contains("TIMEOUT"), "{err}");
        });
    }

    /// The registry seam stays usable: a direct call sees tools registered
    /// after boot (the MCP/plugin registration path).
    #[test]
    fn direct_call_sees_late_registrations() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let late = echo_definition("p1_late", false);
            {
                let inner = host.inner.blocking_lock();
                let inner = inner.as_ref().unwrap();
                let tools: &InMemoryToolRuntime = inner.agent.tools();
                tools.register(late).expect("late register").forget();
            }
            let out = host
                .call_tool("p1_late".into(), serde_json::json!({ "n": 1 }), None, None)
                .expect("late registration is callable");
            assert!(out.ok);
        });
    }
}