//! Git tool: runs git subcommands. Read-only subcommands run in any sandbox
//! mode; write subcommands are refused in read-only mode. Requires approval.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};

/// Git subcommands that mutate the repository. The deny list keeps growing
/// risky verbs; anything obviously mutating a reasonable actor might reach
/// for is here so read-only mode stays honest.
const WRITE_SUBCOMMANDS: &[&str] = &[
    "add",
    "commit",
    "push",
    "pull",
    "fetch",
    "reset",
    "checkout",
    "merge",
    "rebase",
    "revert",
    "cherry-pick",
    "tag",
    "branch",
    "rm",
    "mv",
    "stash",
    "restore",
    "switch",
    "apply",
    "am",
    "clean",
    "config",
    "worktree",
    "notes",
    "update-ref",
    "replace",
    "filter-branch",
    "gc",
    "prune",
    "init",
    "clone",
    "bisect",
    "reflog",
];

fn is_write_subcommand(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    WRITE_SUBCOMMANDS.contains(&first)
}

/// Split a command string into argv honoring single/double quotes (the quote
/// characters group and do not reach git) — `commit -m "fix: x"` must arrive
/// as one `-m` value, not three mangled fragments the way
/// `split_whitespace` produced them.
fn split_argv(command: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    has_token = true;
                }
                c if c.is_whitespace() => {
                    if has_token {
                        argv.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                }
                c => {
                    current.push(c);
                    has_token = true;
                }
            },
        }
    }
    if has_token {
        argv.push(current);
    }
    argv
}

struct Git;

impl ToolExecutor for Git {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let command = json
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| invalid_input("git", "missing command".into()))?
                .to_string();
            if ctx.execution.sandbox_mode == Some(SandboxMode::ReadOnly)
                && is_write_subcommand(&command)
            {
                return Err(tool_failure(
                    "git",
                    "READONLY_MODE",
                    "git write subcommands are refused in read-only sandbox mode".into(),
                ));
            }
            let cwd = ctx
                .execution
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let output = tokio::process::Command::new("git")
                .arg("-c")
                .arg("core.pager=cat")
                .args(split_argv(&command))
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
                .map_err(|e| tool_failure("git", "SPAWN_FAILED", e.to_string()))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let combined = if stderr.is_empty() {
                stdout
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            };
            if output.status.success() {
                Ok(Value::new(serde_json::json!({ "output": combined })))
            } else {
                Err(tool_failure("git", "EXECUTION_FAILED", combined))
            }
        })
    }
}

pub fn git_definition() -> ToolDefinition {
    ToolDefinition {
        name: "git".into(),
        description:
            "Git operations. Parameters: command (log/diff/branch/status/add/commit/changes)".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "path": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["command"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Git),
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

    #[test]
    fn write_subcommand_classification() {
        assert!(is_write_subcommand("commit -m x"));
        assert!(is_write_subcommand("push origin main"));
        assert!(!is_write_subcommand("status"));
    }
}
