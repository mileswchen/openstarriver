//! Write tool: creates or overwrites a file. Refuses in read-only sandbox
//! mode; requires approval (tool-level fact the host trigger asks on).

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{
    append_diff, invalid_input, json_args, resolve_path, text_content, tool_failure,
};

struct Write;

impl ToolExecutor for Write {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            if ctx.execution.sandbox_mode == Some(SandboxMode::ReadOnly) {
                return Err(tool_failure(
                    "write",
                    "READONLY_MODE",
                    "the session is in read-only sandbox mode; switch to workspace-write to modify files".into(),
                ));
            }
            let json = match json_args(&arguments) {
                Ok(json) => json,
                Err(e) => {
                    return Err(e);
                }
            };
            let raw = json
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("write", "missing path".into()))?;
            let content = json
                .get("content")
                .and_then(|c| c.as_str())
                .ok_or_else(|| invalid_input("write", "missing content".into()))?;
            let path = resolve_path(ctx.execution.cwd.as_deref(), raw);
            crate::sandbox::ensure_workspace_writable(
                &path,
                ctx.execution.sandbox_mode,
                ctx.execution.cwd.as_deref(),
            )
            .map_err(|e| tool_failure("write", "OUTSIDE_WORKSPACE", e.to_string()))?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    tool_failure(
                        "write",
                        "MKDIR_FAILED",
                        format!("failed to create directory: {e}"),
                    )
                })?;
            }
            let old = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            tokio::fs::write(&path, content)
                .await
                .map_err(|e| tool_failure("write", "WRITE_FAILED", format!("write failed: {e}")))?;
            let path_str = path.display().to_string();
            let mut output = format!("written: {} ({} bytes)", path_str, content.len());
            append_diff(&mut output, &path_str, &old, content);
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// tool-guide parity (`tool:write` → `write:guide` @ 120).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "write",
        120,
        "Use the write tool to create a new file or completely replace one; it overwrites without confirmation. For targeted changes to an existing file prefer edit, and read the current content first so the replacement keeps everything you are not changing.",
    )
}

pub fn write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "write".into(),
        description: "Create or overwrite file. Parameters: path, content".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file path" },
                "content": { "type": "string", "description": "content to write" }
            },
            "required": ["path", "content"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Write),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: true,
        deny_in_readonly: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;

    async fn run(
        args: serde_json::Value,
        cwd: std::path::PathBuf,
        sandbox_mode: Option<SandboxMode>,
    ) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "write".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        write_definition()
            .executor
            .execute(Value::new(args), ctx)
            .await
    }

    #[tokio::test]
    async fn write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        run(
            serde_json::json!({ "path": "out.txt", "content": "data" }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "data"
        );
    }

    #[tokio::test]
    async fn write_refused_in_read_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "path": "out.txt", "content": "data" }),
            dir.path().to_path_buf(),
            Some(SandboxMode::ReadOnly),
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::Execution { code, .. }) if code == "READONLY_MODE")
        );
        assert!(!dir.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn write_new_file_output_is_all_additions() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "path": "out.txt", "content": "hello\nworld\n" }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("written:"));
        assert!(output.contains("--- "));
        assert!(output.contains("+++ "));
        assert!(output.contains("+hello"));
        assert!(output.contains("+world"));
        assert!(!output
            .lines()
            .any(|l| l.starts_with('-') && !l.starts_with("---")));
    }

    #[tokio::test]
    async fn write_existing_file_output_includes_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.txt"), "one\ntwo\n").unwrap();
        let result = run(
            serde_json::json!({ "path": "out.txt", "content": "one\nTWO\n" }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("-two"));
        assert!(output.contains("+TWO"));
    }

    #[test]
    fn readonly_flag_is_declared() {
        assert!(write_definition().deny_in_readonly);
    }

    /// G0: creating a file outside the workspace under workspace-write → OUTSIDE_WORKSPACE,
    /// the file is not created (design §5A: the error is visible to the model so it can self-correct).
    #[tokio::test]
    async fn write_outside_workspace_rejected_and_file_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let outside = crate::testutil::outside_boundary_dir("write");
        let outside_file = outside.join("intruder.txt");
        let err = run(
            serde_json::json!({ "path": outside_file.to_string_lossy(), "content": "hi" }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap_err();
        let ToolError::Execution { code, message, .. } = err else {
            panic!("expected execution error, got {err:?}");
        };
        assert_eq!(code, "OUTSIDE_WORKSPACE", "{message}");
        assert!(!outside_file.exists(), "file must not be created");
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// G0: relative paths inside the workspace write normally (within the anchored boundary, allowed).
    #[tokio::test]
    async fn write_inside_workspace_allowed_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        run(
            serde_json::json!({ "path": "out/ok.txt", "content": "fine" }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/ok.txt")).unwrap(),
            "fine"
        );
    }
}
