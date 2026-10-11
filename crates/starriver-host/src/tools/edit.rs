//! Edit tool: precision string replacement in one file. Refuses in read-only
//! sandbox mode; requires approval.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{
    append_diff, invalid_input, json_args, resolve_path, text_content, tool_failure,
};

struct Edit;

/// Suggest the file line most similar to the first line of a not-found
/// `old_string`, so the model can recover in one step with the actual
/// current text instead of retrying from stale memory.
fn closest_line_hint(content: &str, probe: &str) -> String {
    if probe.is_empty() {
        return String::new();
    }
    let mut best: Option<(f32, usize, &str)> = None;
    for (idx, line) in content.lines().enumerate() {
        let cand = line.trim();
        if cand.is_empty() {
            continue;
        }
        let ratio = similar::TextDiff::from_chars(probe, cand).ratio();
        if best.is_none() || ratio > best.unwrap().0 {
            best = Some((ratio, idx + 1, line));
        }
    }
    match best {
        Some((ratio, no, line)) if ratio >= 0.6 => {
            let shown: String = line.chars().take(160).collect();
            format!(
                "; closest match at line {no} ({}% similar): {shown:?}",
                (ratio * 100.0) as u32
            )
        }
        _ => String::new(),
    }
}

impl ToolExecutor for Edit {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            if ctx.execution.sandbox_mode == Some(SandboxMode::ReadOnly) {
                return Err(tool_failure(
                    "edit",
                    "READONLY_MODE",
                    "the session is in read-only sandbox mode; switch to workspace-write to modify files".into(),
                ));
            }
            let json = json_args(&arguments)?;
            let raw = json
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("edit", "missing path".into()))?;
            let old = json
                .get("old_string")
                .and_then(|o| o.as_str())
                .ok_or_else(|| invalid_input("edit", "missing old_string".into()))?;
            let new = json
                .get("new_string")
                .and_then(|n| n.as_str())
                .ok_or_else(|| invalid_input("edit", "missing new_string".into()))?;
            let path = resolve_path(ctx.execution.cwd.as_deref(), raw);
            crate::sandbox::ensure_workspace_writable(
                &path,
                ctx.execution.sandbox_mode,
                ctx.execution.cwd.as_deref(),
            )
            .map_err(|e| tool_failure("edit", "OUTSIDE_WORKSPACE", e.to_string()))?;
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| tool_failure("edit", "READ_FAILED", format!("read failed: {e}")))?;
            if !content.contains(old) {
                let probe = old
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or_default();
                let hint = closest_line_hint(&content, probe);
                return Err(tool_failure(
                    "edit",
                    "OLD_STRING_NOT_FOUND",
                    format!(
                        "old_string not found in file; the file may have changed since it was \
                         last read - read it again and retry with exact current text{hint}"
                    ),
                ));
            }
            // The tool's own guide promises "old_string must appear exactly
            // once unless replace_all is true" — and this tool has no
            // replace_all. A blind `replace` silently edited every match,
            // including places the model never read.
            if content.matches(old).count() > 1 {
                return Err(tool_failure(
                    "edit",
                    "AMBIGUOUS_EDIT",
                    format!(
                        "old_string appears {} times; provide more surrounding context so it matches exactly once",
                        content.matches(old).count()
                    ),
                ));
            }
            let updated = content.replace(old, new);
            let path_str = path.display().to_string();
            let mut output = format!("edited {path_str}");
            append_diff(&mut output, &path_str, &content, &updated);
            tokio::fs::write(&path, updated)
                .await
                .map_err(|e| tool_failure("edit", "WRITE_FAILED", format!("write failed: {e}")))?;
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// tool-guide parity (`tool:edit` → `edit:guide` @ 130).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "edit",
        130,
        "Use the edit tool for targeted changes to an existing text file: it replaces a literal old_string with new_string, and old_string must appear exactly once unless replace_all is true. Read the file first so old_string matches the real bytes.",
    )
}

pub fn edit_definition() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        description: "Precision file editing. Parameters: path, old_string, new_string".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            },
            "required": ["path", "old_string", "new_string"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Edit),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: true,
        deny_in_readonly: true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;

    pub(crate) async fn run(
        args: serde_json::Value,
        cwd: std::path::PathBuf,
        sandbox_mode: Option<SandboxMode>,
    ) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "edit".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        edit_definition()
            .executor
            .execute(Value::new(args), ctx)
            .await
    }

    #[tokio::test]
    async fn edit_output_includes_unified_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": "a.txt",
                "old_string": "two",
                "new_string": "TWO"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("edited"));
        assert!(output.contains("--- "));
        assert!(output.contains("+++ "));
        assert!(output.contains("@@"));
        assert!(output.contains("-two"));
        assert!(output.contains("+TWO"));
    }

    #[tokio::test]
    async fn edit_identical_strings_have_no_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "same\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": "a.txt",
                "old_string": "same",
                "new_string": "same"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("edited"));
        assert!(!output.contains("--- "));
    }

    #[tokio::test]
    async fn missing_old_string_error_includes_closest_line_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            "one\n    ctx.fillStyle = \"#f8cccc\";\nthree\n",
        )
        .unwrap();
        let result = run(
            serde_json::json!({
                "path": "a.txt",
                "old_string": "    ctx.fillStyle = \"#bfe3f7\";",
                "new_string": "x"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        let err = result.unwrap_err();
        let ToolError::Execution { message, .. } = err else {
            panic!("expected execution error, got {err:?}");
        };
        assert!(message.contains("read it again"), "got: {message}");
        assert!(
            message.contains("closest match at line 2"),
            "got: {message}"
        );
        assert!(message.contains("#f8cccc"), "got: {message}");
    }

    #[tokio::test]
    async fn missing_old_string_error_has_no_hint_when_nothing_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": "a.txt",
                "old_string": "完全不同的内容 12345 !!!",
                "new_string": "x"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        let err = result.unwrap_err();
        let ToolError::Execution { message, .. } = err else {
            panic!("expected execution error, got {err:?}");
        };
        assert!(message.contains("old_string not found"), "got: {message}");
        assert!(!message.contains("closest match"), "got: {message}");
    }

    #[test]
    fn readonly_flag_is_declared() {
        assert!(edit_definition().deny_in_readonly);
    }

    /// G0: editing a file outside the workspace under workspace-write → OUTSIDE_WORKSPACE,
    /// the target file is not created/modified (design §5A integration test requirement).
    #[tokio::test]
    async fn edit_outside_workspace_rejected_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let outside = crate::testutil::outside_boundary_dir("edit");
        let outside_file = outside.join("victim.txt");
        std::fs::write(&outside_file, "original\n").unwrap();
        let err = run(
            serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "old_string": "original",
                "new_string": "tampered"
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap_err();
        let ToolError::Execution { code, message, .. } = err else {
            panic!("expected execution error, got {err:?}");
        };
        assert_eq!(code, "OUTSIDE_WORKSPACE", "{message}");
        assert!(
            message.contains("outside the workspace root"),
            "error must name the boundary: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "original\n",
            "target file must be untouched"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// G0: `..` escape rejected (relative paths are anchored to the workspace root before the escape check).
    #[tokio::test]
    async fn edit_parent_escape_rejected_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            serde_json::json!({
                "path": "../escape.txt",
                "old_string": "x",
                "new_string": "y"
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap_err();
        let ToolError::Execution { code, .. } = err else {
            panic!("expected execution error");
        };
        assert_eq!(code, "OUTSIDE_WORKSPACE");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    /// G0: temp whitelist allowed (the whitelisted root differs by platform:
    /// POSIX whitelists the literal /tmp — env::temp_dir may be /var/folders
    /// on macOS, outside the whitelist; Windows whitelists %TEMP% itself).
    #[tokio::test]
    async fn edit_tmp_path_allowed_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let tmp_target = std::path::PathBuf::from(format!(
            "/tmp/starriver-edit-e2e-{}.txt",
            std::process::id()
        ));
        #[cfg(windows)]
        let tmp_target =
            std::env::temp_dir().join(format!("starriver-edit-e2e-{}.txt", std::process::id()));
        std::fs::write(&tmp_target, "before\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": tmp_target.to_string_lossy(),
                "old_string": "before",
                "new_string": "after"
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap();
        assert!(result.downcast::<serde_json::Value>().unwrap()["output"]
            .as_str()
            .unwrap()
            .contains("edited"));
        assert_eq!(std::fs::read_to_string(&tmp_target).unwrap(), "after\n");
        let _ = std::fs::remove_file(&tmp_target);
    }

    /// G0: DangerFullAccess is not constrained by the workspace boundary.
    #[tokio::test]
    async fn edit_outside_workspace_allowed_under_danger_full_access() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("ok.txt");
        std::fs::write(&outside_file, "a\n").unwrap();
        run(
            serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "old_string": "a",
                "new_string": "b"
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::DangerFullAccess),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "b\n");
    }
}


// ── BUG-96 regression (round 6) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[tokio::test]
    async fn multiple_matches_are_rejected_not_all_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "x\nx\n").unwrap();
        let err = crate::tools::edit::tests::run(
            serde_json::json!({
                "path": "dup.txt",
                "old_string": "x",
                "new_string": "y",
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.code(),
            "AMBIGUOUS_EDIT",
            "the guide promises exactly-once semantics (got {err:?})"
        );
        let content = std::fs::read_to_string(dir.path().join("dup.txt")).unwrap();
        assert_eq!(content, "x\nx\n", "nothing was silently rewritten");
    }
}
