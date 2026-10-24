//! Bash tool: runs one shell command, optionally confined by the session's
//! sandbox mode. When the OS sandbox is unavailable, workspace-write
//! degrades to unconfined execution under the standing approval policy
//! (read-only stays fail-closed — its deny-all-writes promise cannot be
//! honored unconfined); `STARRIVER_SANDBOX_FAIL_CLOSED=1` restores the
//! original hard failure everywhere.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::sandbox::{
    SandboxError, SandboxMode, SandboxPolicy, SandboxProvider, SpawnHook,
};
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
use starriver_sandbox::denial;

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::sandbox::SystemSandbox;

/// BUG-101 defaults: the model-requested timeout is clamped to
/// `[default, max]` — absent → default (120s), above the cap → the cap
/// (1800s, `maxTimeoutMs` parity). Settings-tunable via
/// `agent.bash_timeout_secs` / `agent.bash_timeout_max_secs`.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_BASH_TIMEOUT_MAX_SECS: u64 = 1800;

/// Per-executor timeout limits, injected from settings at registration time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BashTimeoutLimits {
    pub default_secs: u64,
    pub max_secs: u64,
}

/// SIGKILL the child's whole process group when dropped while armed.
/// `kill_on_drop` only reaches the direct child; when the wait future is
/// dropped by a turn cancel, grandchildren (npm → electron-builder →
/// hdiutil…) would otherwise escape, orphan to launchd, and keep holding
/// locks that poison later builds. Armed at spawn, disarmed on normal
/// completion and on the explicit timeout kill so a finished call never
/// fires a stray killpg at a possibly recycled pgid.
#[cfg(unix)]
struct ProcessGroupKill {
    pid: Option<u32>,
}

#[cfg(unix)]
impl ProcessGroupKill {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

impl Default for BashTimeoutLimits {
    fn default() -> Self {
        Self {
            default_secs: DEFAULT_BASH_TIMEOUT_SECS,
            max_secs: DEFAULT_BASH_TIMEOUT_MAX_SECS,
        }
    }
}

/// Resolve the effective timeout: model-requested value clamped to the hard
/// cap; absent (or the round-8 `timeout: 0` compatibility case) → default.
fn resolve_timeout_secs(requested: Option<u64>, limits: &BashTimeoutLimits) -> u64 {
    match requested {
        Some(secs) if secs > 0 => secs.min(limits.max_secs),
        _ => limits.default_secs,
    }
}

struct Bash {
    limits: BashTimeoutLimits,
    /// BUG-157: settings-provided tool environment (`agent.tool_env`), already
    /// `$VAR`-expanded by [`expand_tool_env`] at registration time. `None`
    /// keeps the historical behavior byte-for-byte (child inherits the daemon
    /// environment untouched).
    tool_env: Option<std::collections::BTreeMap<String, String>>,
}

impl ToolExecutor for Bash {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let limits = self.limits;
        let tool_env = self.tool_env.clone();
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let command = json
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| invalid_input("bash", "missing command".into()))?
                .to_string();
            let requested = json
                .get("timeout")
                .and_then(|t| t.as_f64().map(|f| f as u64).or_else(|| t.as_u64()));
            let timeout_secs = resolve_timeout_secs(requested, &limits);
            let cwd = ctx
                .execution
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            let mut argv = vec!["sh".to_string(), "-c".to_string(), command.clone()];
            let mut runner = "none".to_string();
            let mut spawn_hook: Option<SpawnHook> = None;
            let mut degraded_notice: Option<String> = None;
            if let Some(mode) = ctx.execution.sandbox_mode {
                if mode != SandboxMode::DangerFullAccess {
                    // Root promotion keeps worktree sessions writing inside
                    // their workspace (and the shared main repo .git) while
                    // the process cwd sits in <workspace>/.worktrees/….
                    let workspace_root = crate::sandbox::effective_workspace_root(&cwd);
                    let policy = SandboxPolicy {
                        mode,
                        workspace_root: workspace_root.to_string_lossy().into_owned(),
                        session_id: ctx.execution.agent_id.clone(),
                    };
                    match SystemSandbox::new().confine(argv.clone(), &policy) {
                        Ok(confined_argv) => {
                            // BUG-101: commands driving macOS system XPC
                            // services freeze inside the sandbox (the request
                            // never fails nor returns) — refuse up front.
                            if super::system_service::command_needs_system_services(&command) {
                                return Err(tool_failure(
                                    "bash",
                                    "SANDBOX_SYSTEM_SERVICE",
                                    "command requires macOS system services that are \
                                     unavailable inside the sandbox (it would hang); run \
                                     it outside the sandboxed shell"
                                        .to_string(),
                                ));
                            }
                            argv = confined_argv.argv;
                            runner = confined_argv.runner;
                            spawn_hook = confined_argv.spawn_hook;
                        }
 // Degraded execution (-style): with no OS
                        // sandbox the standing approval policy is the only
                        // protection, so the command still runs — loudly.
                        // Read-only keeps failing closed, and any host can
                        // opt back into strict behavior via the env switch.
                        Err(e @ SandboxError::Unavailable { .. })
                            if mode == SandboxMode::WorkspaceWrite && !fail_closed_requested() =>
                        {
                            tracing::warn!(
                                "sandbox unavailable; bash degraded to approval-only: {e}"
                            );
                            degraded_notice = Some(format!(
                                "[sandbox unavailable: {e} — ran unconfined under \
                                 approval-only protection]"
                            ));
                        }
                        Err(e) => {
                            return Err(tool_failure("bash", e.code(), e.to_string()));
                        }
                    }
                }
            }

            let mut cmd = tokio::process::Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            // BUG-101: put the child in its own process group so a timeout
            // can kill the whole tree — the direct child alone leaves
            // grandchildren (npm → electron-builder → hdiutil…) alive, and
            // respawn loops then outlive the tool call. Unavailable on
            // non-unix; there kill_on_drop remains the only lever. Applied
            // in every mode: a hung command is not a sandbox-only problem.
            #[cfg(unix)]
            cmd.process_group(0);
            // BUG-157: inject the settings-provided tool environment
            // (`agent.tool_env`, expanded at registration) before any
            // in-process confinement hook — the hook only pre-opens paths
            // and does not touch the environment, so the order is inert.
            // This changes only the child's environment, never the argv
            // sandbox verdicts above.
            if let Some(env) = &tool_env {
                for (key, value) in env {
                    cmd.env(key, value);
                }
            }
            if let Some(hook) = spawn_hook {
                // In-process confinement (Landlock): applies inside the child
                // right before exec; failure = spawn fails, never run unconfined.
                hook(&mut cmd).map_err(|e| {
                    tool_failure(
                        "bash",
                        "SANDBOX_CONFINE_FAILED",
                        format!("sandbox enforcement failed: {e}"),
                    )
                })?;
            }
            // BUG-101: spawn explicitly so the child pid is known — the
            // timeout path kills the child's whole process group (with
            // `process_group(0)` the group id == the child pid).
            let mut child = cmd.spawn().map_err(|e| {
                tool_failure("bash", "SPAWN_FAILED", e.to_string())
            })?;
            let child_pid = child.id();
            // Armed for every non-normal exit from the wait below: the timeout
            // branch disarms after its explicit killpg, and a drop of the
            // wait future (turn cancel reaps the bash dispatch) fires the
            // guard's own killpg — that is the cancellation cleanup path.
            #[cfg(unix)]
            let mut group_kill = ProcessGroupKill { pid: child_pid };
            let output = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                child.wait_with_output(),
            )
            .await
            {
                Ok(result) => {
                    #[cfg(unix)]
                    group_kill.disarm();
                    result.map_err(|e| tool_failure("bash", "SPAWN_FAILED", e.to_string()))?
                }
                Err(_) => {
                    #[cfg(unix)]
                    if let Some(pid) = child_pid {
                        unsafe {
                            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                        }
                        // The guard would repeat the killpg on drop; disarm so
                        // the group id (leader now dead) cannot be recycled
                        // onto an unrelated process between here and drop.
                        group_kill.disarm();
                    }
                    return Err(tool_failure(
                        "bash",
                        "TIMEOUT",
                        format!(
                            "command timed out after {timeout_secs}s (the whole process \
                             tree was terminated)"
                        ),
                    ));
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let mut combined = if stderr.is_empty() {
                stdout.clone()
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            };
            if let Some(notice) = &degraded_notice {
                combined = format!("{notice}\n{combined}");
            }
 // every settled command result ends with an
            // `[exit code: N]` marker so the model can mechanically check
            // success before moving on (bash:guide points at it).
            let exit_marker = format!("[exit code: {}]", output.status.code().unwrap_or(-1));
            if output.status.success() {
                Ok(Value::new(
                    serde_json::json!({ "output": format!("{combined}\n{exit_marker}") }),
                ))
            } else if denial::runner_startup_failed(&runner, &stdout, &stderr) {
                // Backend broke mid-session: fail closed, never run unconfined.
                Err(tool_failure(
                    "bash",
                    "SANDBOX_CONFINE_FAILED",
                    format!("sandbox backend failed to start: {stderr}"),
                ))
            } else if let Some(sig) = denial::classify(&runner, &stderr) {
                Err(tool_failure(
                    "bash",
                    "SANDBOX_DENIED",
                    format!("{}\n[stderr]\n{stderr}", denial::message(&sig)),
                ))
            } else {
                Err(tool_failure(
                    "bash",
                    "EXECUTION_FAILED",
                    format!("{combined}\n{exit_marker}"),
                ))
            }
        })
    }
}

/// Restores the pre-degradation fail-closed behavior for compliance-
/// sensitive hosts: with `STARRIVER_SANDBOX_FAIL_CLOSED=1|true` every
/// sandbox-unavailable confined call is a hard error again.
fn fail_closed_requested() -> bool {
    std::env::var("STARRIVER_SANDBOX_FAIL_CLOSED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// tool-guide parity (`tool:bash` @ 1000 → `bash:guide` @ 100): the
/// exit-code marker sentence is the original; the prefer-dedicated-tools
/// line is starriver's addition (our fs tools carry their own guides).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "bash",
        100,
        "Check the [exit code: N] marker on every bash result; investigate \
         failures before moving on. For file work prefer the dedicated tools: \
         read to inspect files, glob to find files by name, grep to search \
         contents, write and edit to change them.",
    )
}

pub fn bash_definition() -> ToolDefinition {
    bash_definition_with_limits(BashTimeoutLimits::default())
}

/// BUG-101: settings-tunable variant — `plugin_kernel` injects
/// `agent.bash_timeout_secs` / `agent.bash_timeout_max_secs` here.
pub fn bash_definition_with_limits(limits: BashTimeoutLimits) -> ToolDefinition {
    bash_definition_with_env(limits, None)
}

/// BUG-157: settings-tunable tool environment — `plugin_kernel` injects the
/// already-expanded `agent.tool_env` map here (`None` = inherit the daemon
/// environment, the historical behavior).
pub fn bash_definition_with_env(
    limits: BashTimeoutLimits,
    tool_env: Option<std::collections::BTreeMap<String, String>>,
) -> ToolDefinition {
    let mut definition = bash_definition_base();
    definition.executor = Arc::new(Bash { limits, tool_env });
    definition
}

/// BUG-157: one-level `$VAR` / `${VAR}` expansion against the daemon process
/// environment, applied to `agent.tool_env` values once at registration time.
/// Undefined variables expand to the empty string (warned); there is no
/// recursive expansion and no shell quoting — values are used literally.
/// Keys that are empty or contain `=` / NUL are dropped with a warning
/// (they would make `Command::env` misbehave or panic downstream).
pub fn expand_tool_env(
    map: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (key, value) in map {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            tracing::warn!("agent.tool_env: dropping invalid key {key:?}");
            continue;
        }
        out.insert(key.clone(), expand_env_vars(value));
    }
    out
}

fn expand_env_vars(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let (name, consumed) = if bytes[i + 1] == b'{' {
                match value[i + 2..].find('}') {
                    Some(end) => (value[i + 2..i + 2 + end].to_string(), end + 3),
                    None => {
                        // Unterminated `${` — literal `$`.
                        result.push('$');
                        i += 1;
                        continue;
                    }
                }
            } else {
                let rest = &value[i + 1..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                (rest[..end].to_string(), end + 1)
            };
            if name.is_empty() {
                // Bare `$` — literal.
                result.push('$');
                i += 1;
                continue;
            }
            match std::env::var(&name) {
                Ok(v) => result.push_str(&v),
                Err(_) => {
                    tracing::warn!(
                        "agent.tool_env: $NAME reference to unset variable {name} expands to empty"
                    );
                }
            }
            i += consumed;
        } else {
            // Copy one full UTF-8 scalar so multi-byte content survives.
            let ch_len = value[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            result.push_str(&value[i..i + ch_len]);
            i += ch_len;
        }
    }
    result
}

fn bash_definition_base() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        description: "Execute a shell command on the device".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "shell command to execute" },
                "timeout": { "type": "integer", "description": "timeout seconds (clamped to the configured maximum)" }
            },
            "required": ["command"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Bash { limits: BashTimeoutLimits::default(), tool_env: None }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: true,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;

    fn marker_execution(command: &str) -> ToolRunContext {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": command })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(execution)
    }

    async fn run(definition: &ToolDefinition, command: &str) -> String {
        let result = definition
            .executor
            .execute(
                Value::new(serde_json::json!({ "command": command })),
                marker_execution(command),
            )
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        json["output"].as_str().unwrap().to_string()
    }

    /// BUG-157: `agent.tool_env` values reach the child process with one-level
    /// `$VAR` expansion applied at registration time.
    #[tokio::test]
    async fn tool_env_injects_expanded_values() {
        let home = std::env::var("HOME").unwrap_or_default();
        let env = expand_tool_env(&std::collections::BTreeMap::from([
            ("H2T_MARKER".to_string(), "yes".to_string()),
            ("H2T_EXP".to_string(), "$HOME/.cargo/bin:$PATH".to_string()),
            ("H2T_BRACE".to_string(), "${HOME}/x".to_string()),
        ]));
        // One-level expansion resolves BOTH $HOME and $PATH against the
        // daemon process env at registration time.
        assert_eq!(
            env.get("H2T_EXP").unwrap(),
            &format!(
                "{home}/.cargo/bin:{}",
                std::env::var("PATH").unwrap_or_default()
            )
        );
        assert_eq!(env.get("H2T_BRACE").unwrap(), &format!("{home}/x"));

        let definition =
            bash_definition_with_env(BashTimeoutLimits::default(), Some(env));
        let out = run(&definition, "echo $H2T_MARKER").await;
        assert!(out.contains("yes"), "marker missing: {out}");

        // The expanded value survived into the child: HOME expansion literal
        // in the map, PATH baked in at registration.
        let out = run(
            &definition,
            r#"case "$H2T_EXP" in "$HOME/.cargo/bin:"*) echo expanded;; *) echo "got:$H2T_EXP";; esac"#,
        )
        .await;
        assert!(out.contains("expanded"), "expansion missing in child: {out}");
    }

    /// BUG-157: unset variables expand to empty; invalid keys are dropped.
    #[test]
    fn expand_tool_env_handles_edge_cases() {
        let env = expand_tool_env(&std::collections::BTreeMap::from([
            ("GOOD".to_string(), "a${H2T_UNSET_XYZ}b".to_string()),
            ("BARE".to_string(), "cost: 5$ and 10¢".to_string()),
            (String::new(), "v".to_string()),
            ("BAD=KEY".to_string(), "v".to_string()),
            ("WITH\0NUL".to_string(), "v".to_string()),
        ]));
        assert_eq!(env.get("GOOD").unwrap(), "ab");
        assert_eq!(env.get("BARE").unwrap(), "cost: 5$ and 10¢");
        assert!(env.get("").is_none() && env.get("BAD=KEY").is_none());
        assert!(env.iter().all(|(k, _)| !k.contains('\0')));
    }

    /// BUG-157 regression lock: without tool_env the child sees no extra
    /// variables (historical byte-for-byte behavior).
    #[tokio::test]
    async fn no_tool_env_keeps_historical_behavior() {
        let definition = bash_definition_with_limits(BashTimeoutLimits::default());
        let out = run(&definition, "echo ${H2T_MARKER:-unset}").await;
        assert!(out.contains("unset"), "expected bare env: {out}");
    }

    #[tokio::test]
    async fn bash_runs_simple_command_unconfined() {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": "echo hi" })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let result = bash_definition()
            .executor
            .execute(Value::new(serde_json::json!({ "command": "echo hi" })), ctx)
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"].as_str().unwrap().contains("hi"));
    }

    /// Cancellation cleanup: dropping the execute future mid-run (a turn
    /// cancel reaps the dispatch) must SIGKILL the whole process group.
    /// kill_on_drop alone only reaches the direct child — grandchildren
    /// escape to launchd and keep holding locks (the wedged-hdiutil incident).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_execute_future_kills_process_group() {
        let marker = std::env::temp_dir().join(format!(
            "starriver-pgtest-{}-{}.pid",
            std::process::id(),
            line!()
        ));
        let marker_str = marker.display();
        let command = format!("sleep 987 & echo $! > {marker_str}; sleep 987");

        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": command })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        // Run the tool on a real task; abort() drops the future at its await
        // point — the same mid-run drop a turn cancel performs.
        let handle = tokio::spawn(bash_definition().executor.execute(
            Value::new(serde_json::json!({ "command": command })),
            ctx,
        ));
        // Wait for the grandchild to be up and its pid recorded. Generous
        // window: under the full suite's parallel load sh startup can take
        // seconds, and this check must not be the flaky part.
        let mut grandchild = None;
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                grandchild = text.trim().parse::<i32>().ok();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let grandchild = grandchild.expect("grandchild pid marker never appeared");
        // The turn-cancel shape: the dispatch future is dropped mid-run.
        handle.abort();
        // SIGKILL is immediate but reaping by launchd is not; poll for gone.
        let mut gone = false;
        for _ in 0..300 {
            let alive = unsafe { libc::kill(grandchild, 0) == 0 };
            if !alive {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(&marker);
        assert!(gone, "grandchild {grandchild} survived the drop kill");
    }

 /// every settled result ends with `[exit code: N]` so the
    /// model can mechanically check success (bash:guide points at it).
    #[tokio::test]
    async fn success_output_ends_with_exit_code_marker() {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": "echo hi" })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let result = bash_definition()
            .executor
            .execute(Value::new(serde_json::json!({ "command": "echo hi" })), ctx)
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(
            output.trim_end().ends_with("[exit code: 0]"),
            "missing marker: {output:?}"
        );
    }

    #[tokio::test]
    async fn failed_command_carries_the_real_exit_code() {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({
                "command": "echo boom; exit 3"
            })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let err = bash_definition()
            .executor
            .execute(
                Value::new(serde_json::json!({ "command": "echo boom; exit 3" })),
                ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "EXECUTION_FAILED");
        let starriver_kernel::tools::ToolError::Execution { message, .. } = &err else {
            panic!("execution failure expected: {err:?}");
        };
        assert!(message.contains("boom"), "output must survive: {message:?}");
        assert!(
            message.trim_end().ends_with("[exit code: 3]"),
            "missing marker: {message:?}"
        );
    }

    /// Degradation tests share process-global env + the sandbox probe's
    /// OnceLock cache, so they serialize on this lock and all force the
    /// same deterministic unavailability (`STARRIVER_SANDBOX_RUNNER=bogus`
    /// is rejected by every platform's select_runner). Poison-tolerant:
    /// one sibling's assertion failure must not cascade into the others.
    static DEGRADE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn confined_execution(mode: SandboxMode) -> ToolRunContext {
        ToolRunContext::new(ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": "echo hi" })),
            agent_id: None,
            cwd: None,
            sandbox_mode: Some(mode),
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        })
    }

    #[tokio::test]
    async fn workspace_write_degrades_to_unconfined_when_sandbox_unavailable() {
        let _guard = DEGRADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("STARRIVER_SANDBOX_RUNNER", "bogus");
        std::env::remove_var("STARRIVER_SANDBOX_FAIL_CLOSED");
        let ctx = confined_execution(SandboxMode::WorkspaceWrite);
        let result = bash_definition()
            .executor
            .execute(Value::new(serde_json::json!({ "command": "echo hi" })), ctx)
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("hi"), "command must still run: {output:?}");
        assert!(
            output.contains("[sandbox unavailable"),
            "degradation must be visible in the result: {output:?}"
        );
        assert!(
            output.trim_end().ends_with("[exit code: 0]"),
            "missing marker: {output:?}"
        );
        // Restore process-global state for concurrently running tests.
        std::env::remove_var("STARRIVER_SANDBOX_RUNNER");
        std::env::remove_var("STARRIVER_SANDBOX_FAIL_CLOSED");
    }

    #[tokio::test]
    async fn readonly_stays_fail_closed_when_sandbox_unavailable() {
        let _guard = DEGRADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("STARRIVER_SANDBOX_RUNNER", "bogus");
        std::env::remove_var("STARRIVER_SANDBOX_FAIL_CLOSED");
        let ctx = confined_execution(SandboxMode::ReadOnly);
        let err = bash_definition()
            .executor
            .execute(Value::new(serde_json::json!({ "command": "echo hi" })), ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SANDBOX_UNAVAILABLE");
        // Restore process-global state for concurrently running tests.
        std::env::remove_var("STARRIVER_SANDBOX_RUNNER");
        std::env::remove_var("STARRIVER_SANDBOX_FAIL_CLOSED");
    }

    // ── BUG-101: timeout clamp, process-group kill, system-service refusal ──

    #[test]
    fn timeout_clamp_default_cap_and_zero_compat() {
        let limits = BashTimeoutLimits { default_secs: 120, max_secs: 1800 };
        // Absent → default.
        assert_eq!(resolve_timeout_secs(None, &limits), 120);
        // Round-8 compat: timeout 0 / negative junk → default, never instant.
        assert_eq!(resolve_timeout_secs(Some(0), &limits), 120);
        // Model passthrough within the cap.
        assert_eq!(resolve_timeout_secs(Some(300), &limits), 300);
        // Above the hard cap → clamped.
        assert_eq!(resolve_timeout_secs(Some(999999), &limits), 1800);
    }

    /// The timeout must kill the WHOLE process tree: the command backgrounds
    /// a sleeper and waits; after the tool times out the sleeper (a
    /// grandchild in the child's process group) must be gone too.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        // Unique marker so pgrep only ever matches THIS run's sleeper —
        // stale sleepers from earlier (pre-fix) runs must not pollute it.
        let marker = format!("starriver-sleeper-{}", std::process::id());
        let command = format!("exec -a {marker} sleep 300 & wait");
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({
                "command": command,
                "timeout": 1
            })),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let err = bash_definition()
            .executor
            .execute(
                Value::new(
                    serde_json::json!({ "command": command, "timeout": 1 }),
                ),
                ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TIMEOUT");
        let starriver_kernel::tools::ToolError::Execution { message, .. } = &err else {
            panic!("execution failure expected: {err:?}");
        };
        assert!(message.contains("process tree"), "{message:?}");
        // Give the kernel a moment to reap; then the marked sleeper — a
        // grandchild in the child's process group — must be gone too.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let leftover = std::process::Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .expect("pgrep must run");
        assert!(
            !leftover.status.success(),
            "sleeper must have been killed with its process tree: {}",
            String::from_utf8_lossy(&leftover.stdout)
        );
    }

    /// Confined execution refuses system-service commands up front
    /// (SANDBOX_SYSTEM_SERVICE) instead of freezing on a blocked XPC call.
    #[cfg(unix)]
    #[tokio::test]
    async fn confined_system_service_command_fails_fast() {
        // Only meaningful when the seatbelt backend is actually available;
        // skip silently otherwise (CI windows/missing sandbox-exec hosts).
        if SystemSandbox::new()
            .confine(
                vec!["true".into()],
                &SandboxPolicy {
                    mode: SandboxMode::WorkspaceWrite,
                    workspace_root: std::env::temp_dir().to_string_lossy().into_owned(),
                    session_id: None,
                },
            )
            .is_err()
        {
            return;
        }
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(serde_json::json!({ "command": "hdiutil list" })),
            agent_id: None,
            cwd: None,
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let err = bash_definition()
            .executor
            .execute(
                Value::new(serde_json::json!({ "command": "hdiutil list" })),
                ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SANDBOX_SYSTEM_SERVICE");
    }

    #[tokio::test]
    async fn fail_closed_env_restores_hard_error() {
        let _guard = DEGRADE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("STARRIVER_SANDBOX_RUNNER", "bogus");
        std::env::set_var("STARRIVER_SANDBOX_FAIL_CLOSED", "1");
        let ctx = confined_execution(SandboxMode::WorkspaceWrite);
        let err = bash_definition()
            .executor
            .execute(Value::new(serde_json::json!({ "command": "echo hi" })), ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SANDBOX_UNAVAILABLE");
    }
}
