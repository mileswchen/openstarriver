//! Read tool: reads a file (optionally offset/limit), resolving paths against
//! the session cwd.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, resolve_path, text_content, tool_failure};

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

struct Read;

impl ToolExecutor for Read {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let raw = json
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("read", "missing path".into()))?;
            let path = resolve_path(ctx.execution.cwd.as_deref(), raw);
            if !path.exists() {
                return Err(tool_failure(
                    "read",
                    "NOT_FOUND",
                    format!("file not found: {}", path.display()),
                ));
            }
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| tool_failure("read", "READ_FAILED", format!("read failed: {e}")))?;
            if meta.len() > MAX_FILE_SIZE {
                return Err(tool_failure(
                    "read",
                    "FILE_TOO_LARGE",
                    format!(
                        "file too large: {} bytes > limit {MAX_FILE_SIZE}",
                        meta.len()
                    ),
                ));
            }
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| tool_failure("read", "READ_FAILED", format!("read failed: {e}")))?;
            // offset/limit are declared in the schema and advertised by the
            // guide ("page through large files") — implement them as line
            // windowing so paging actually pages instead of re-returning the
            // whole file.
            let offset = json.get("offset").and_then(|o| o.as_u64());
            let limit = json.get("limit").and_then(|l| l.as_u64());
            let output = match (offset, limit) {
                (None, None) => content,
                _ => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = offset.unwrap_or(0) as usize;
                    let start = start.min(lines.len());
                    let end = match limit {
                        Some(limit) => start.saturating_add(limit as usize).min(lines.len()),
                        None => lines.len(),
                    };
                    lines[start..end].join("\n")
                }
            };
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// tool-guide parity (`tool:read` → `read:guide` @ 110).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "read",
        110,
        "Use the read tool — not shell commands like cat or head — to inspect files. Use offset and limit to page through large files.",
    )
}

pub fn read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read".into(),
        description:
            "Read file content. Parameters: path (file path), offset (optional), limit (optional)"
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file path" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Read),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;

    async fn run(args: serde_json::Value, cwd: std::path::PathBuf) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "read".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let definition = read_definition();
        definition.executor.execute(Value::new(args), ctx).await
    }

    #[tokio::test]
    async fn read_existing_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello").unwrap();
        let result = run(
            serde_json::json!({ "path": "a.txt" }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert_eq!(json["output"], "hello");
    }

    #[tokio::test]
    async fn read_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "path": "missing.txt" }),
            dir.path().to_path_buf(),
        )
        .await;
        assert!(matches!(result, Err(ToolError::Execution { code, .. }) if code == "NOT_FOUND"));
    }

    #[test]
    fn readonly_flag_is_declared() {
        assert!(!read_definition().deny_in_readonly);
    }
}
